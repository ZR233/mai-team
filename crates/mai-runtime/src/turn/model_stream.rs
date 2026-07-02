use std::sync::Arc;

use futures::StreamExt;
use mai_protocol::{
    AgentId, MessageRole, ModelOutputItem, ModelResponse, ServiceEventKind, SessionId,
    ToolDefinition, TurnId,
};
use mai_store::{ConfigStore, ProviderSelection};
use pl_core::CoreSession;
use pl_model::{CompletionEventStream, CompletionResponse, CompletionStreamAccumulator, ToolCall};
use pl_protocol::{Message, PureError};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::events::RuntimeEvents;
use crate::state::AgentRecord;
use crate::{ModelClient, completion_response_to_model_response};
use crate::{Result, RuntimeError};

#[derive(Debug)]
pub(crate) struct TurnModelContext {
    pub(crate) provider_id: String,
    pub(crate) model_name: String,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) provider_selection: ProviderSelection,
    pub(crate) tools: Vec<ToolDefinition>,
    pub(crate) instructions: String,
}

#[derive(Debug)]
pub(crate) struct ModelTurnResult {
    pub(crate) response: ModelResponse,
    pub(crate) tool_calls: Vec<(String, String, Value)>,
    pub(crate) last_assistant_text: Option<String>,
    pub(crate) made_progress: bool,
}

pub(crate) struct TurnStreamContext<'a> {
    pub(crate) store: &'a ConfigStore,
    pub(crate) events: &'a RuntimeEvents,
    pub(crate) agent: &'a Arc<AgentRecord>,
    pub(crate) agent_id: AgentId,
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
}

pub(crate) async fn run_model_stream_turn(
    model: &ModelClient,
    ctx: &TurnStreamContext<'_>,
    model_context: &TurnModelContext,
    session: &mut CoreSession,
    cancellation_token: &CancellationToken,
) -> Result<ModelTurnResult> {
    session.set_prompt_cache_key(format!("agent:{}:session:{}", ctx.agent_id, ctx.session_id));
    let stream = model
        .open_completion_event_stream(
            &model_context.provider_selection,
            model_context.reasoning_effort.as_deref(),
            &model_context.instructions,
            &model_context.tools,
            session,
        )
        .await?;
    consume_turn_stream(model, ctx, session, stream, cancellation_token).await
}

pub(crate) async fn consume_model_stream_to_response(
    model: &ModelClient,
    provider_selection: &ProviderSelection,
    reasoning_effort: Option<&str>,
    instructions: &str,
    input: &[Message],
    tools: &[ToolDefinition],
    cancellation_token: &CancellationToken,
) -> std::result::Result<ModelResponse, PureError> {
    let mut session = CoreSession::from_messages(input.to_vec());
    let response = model
        .stream_session_completion_response(
            provider_selection,
            reasoning_effort,
            instructions,
            tools,
            &mut session,
            cancellation_token,
        )
        .await?;
    Ok(completion_response_to_model_response(response))
}

async fn consume_turn_stream(
    model: &ModelClient,
    ctx: &TurnStreamContext<'_>,
    session: &mut CoreSession,
    mut stream: CompletionEventStream,
    cancellation_token: &CancellationToken,
) -> Result<ModelTurnResult> {
    let (event_tx, _event_rx) = tokio::sync::broadcast::channel(32);
    let mut accumulator = CompletionStreamAccumulator::new(None);
    while let Some(event) = stream.next().await {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        let event = event?;
        accumulator.apply(event, &event_tx)?;
    }
    let response = accumulator.finish(&event_tx)?;
    let result = record_completion_response(ctx, session, response).await?;
    model.apply_completed_state(session, result.response.id.as_deref());
    Ok(result)
}

async fn record_completion_response(
    ctx: &TurnStreamContext<'_>,
    session: &mut CoreSession,
    response: CompletionResponse,
) -> Result<ModelTurnResult> {
    let response = completion_response_to_model_response(response);
    let mut tool_calls = Vec::new();
    let mut last_assistant_text = None;
    let mut made_progress = false;
    for item in &response.output {
        match item {
            ModelOutputItem::Reasoning { content } => {
                if !content.trim().is_empty() {
                    made_progress = true;
                    record_reasoning_item(ctx, content.clone()).await?;
                    session.push_assistant_response(String::new(), Some(content.clone()));
                }
            }
            ModelOutputItem::Message { text } => {
                if !text.trim().is_empty() {
                    made_progress = true;
                    last_assistant_text = Some(text.clone());
                    record_assistant_message(ctx, text.clone()).await?;
                    session.push_assistant_response(text.clone(), None);
                }
            }
            ModelOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                raw_arguments,
            } => {
                made_progress = true;
                let call_id = normalized_call_id(call_id.clone());
                ctx.events
                    .publish(ServiceEventKind::ToolCallDelta {
                        agent_id: ctx.agent_id,
                        session_id: Some(ctx.session_id),
                        turn_id: ctx.turn_id,
                        call_id: call_id.clone(),
                        tool_name: name.clone(),
                        arguments_delta: raw_arguments.clone(),
                    })
                    .await;
                super::history::record_history_item(
                    ctx.store,
                    ctx.agent,
                    ctx.agent_id,
                    ctx.session_id,
                    super::history::tool_call_message(
                        call_id.clone(),
                        name.clone(),
                        raw_arguments.clone(),
                    ),
                )
                .await?;
                session.push_assistant_tool_calls(
                    None,
                    vec![ToolCall::function(
                        call_id.clone(),
                        name.clone(),
                        arguments.clone(),
                        Some(call_id.clone()),
                    )],
                    None,
                );
                tool_calls.push((call_id, name.clone(), arguments.clone()));
            }
            ModelOutputItem::Other { .. } => {}
        }
    }
    Ok(ModelTurnResult {
        response,
        tool_calls,
        last_assistant_text,
        made_progress,
    })
}

async fn record_reasoning_item(ctx: &TurnStreamContext<'_>, content: String) -> Result<()> {
    super::history::record_history_item(
        ctx.store,
        ctx.agent,
        ctx.agent_id,
        ctx.session_id,
        super::history::reasoning_message(content.clone()),
    )
    .await?;
    ctx.events
        .publish(ServiceEventKind::ReasoningCompleted {
            agent_id: ctx.agent_id,
            session_id: Some(ctx.session_id),
            turn_id: ctx.turn_id,
            message_id: format!("reasoning_{}", Uuid::new_v4()),
            content,
        })
        .await;
    Ok(())
}

async fn record_assistant_message(ctx: &TurnStreamContext<'_>, text: String) -> Result<()> {
    super::history::record_message(
        ctx.store,
        ctx.agent,
        ctx.agent_id,
        ctx.session_id,
        MessageRole::Assistant,
        text.clone(),
    )
    .await?;
    super::history::record_history_item(
        ctx.store,
        ctx.agent,
        ctx.agent_id,
        ctx.session_id,
        super::history::assistant_text_message(text.clone()),
    )
    .await?;
    let message_id = format!("msg_{}", Uuid::new_v4());
    ctx.events
        .publish(ServiceEventKind::AgentMessageCompleted {
            agent_id: ctx.agent_id,
            session_id: Some(ctx.session_id),
            turn_id: ctx.turn_id,
            message_id: message_id.clone(),
            role: MessageRole::Assistant,
            channel: "final".to_string(),
            content: text.clone(),
        })
        .await;
    ctx.events
        .publish(ServiceEventKind::AgentMessage {
            agent_id: ctx.agent_id,
            session_id: Some(ctx.session_id),
            turn_id: Some(ctx.turn_id),
            role: MessageRole::Assistant,
            content: text,
        })
        .await;
    Ok(())
}

fn normalized_call_id(call_id: String) -> String {
    if call_id.is_empty() {
        format!("call_{}", Uuid::new_v4())
    } else {
        call_id
    }
}
