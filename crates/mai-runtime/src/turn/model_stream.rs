use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use mai_model::{
    ModelClient, ModelEventStream, ModelStreamAccumulator, ModelStreamEvent, ModelTurnState,
};
use mai_protocol::{
    AgentId, MessageRole, ModelInputItem, ModelOutputItem, ModelResponse, ModelToolCall,
    ServiceEventKind, SessionId, ToolDefinition, TurnId,
};
use mai_store::{ConfigStore, ProviderSelection};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::events::RuntimeEvents;
use crate::state::AgentRecord;
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

#[derive(Debug, Clone, Default)]
struct StreamToolPreview {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
}

pub(crate) async fn run_model_stream_turn(
    model: &ModelClient,
    store: &ConfigStore,
    events: &RuntimeEvents,
    agent: &Arc<AgentRecord>,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    model_context: &TurnModelContext,
    history: &[ModelInputItem],
    turn_model_state: &mut ModelTurnState,
    cancellation_token: &CancellationToken,
) -> Result<ModelTurnResult> {
    let resolved = model.resolve(
        &model_context.provider_selection.provider,
        &model_context.provider_selection.model,
        model_context.reasoning_effort.as_deref(),
    );
    let stream = model
        .send_turn(
            &resolved,
            &model_context.instructions,
            history,
            &model_context.tools,
            turn_model_state,
            cancellation_token,
        )
        .await
        .map_err(model_error_to_runtime)?;
    consume_turn_stream(
        model,
        store,
        events,
        agent,
        agent_id,
        session_id,
        turn_id,
        turn_model_state,
        stream,
        cancellation_token,
    )
    .await
}

pub(crate) async fn consume_model_stream_to_response(
    model: &ModelClient,
    resolved: &mai_model::ResolvedProvider,
    instructions: &str,
    input: &[ModelInputItem],
    tools: &[ToolDefinition],
    state: &mut ModelTurnState,
    cancellation_token: &CancellationToken,
) -> std::result::Result<ModelResponse, mai_model::ModelError> {
    let mut stream = model
        .send_turn(
            resolved,
            instructions,
            input,
            tools,
            state,
            cancellation_token,
        )
        .await?;
    let mut accumulator = ModelStreamAccumulator::default();
    while let Some(event) = stream.next().await {
        if cancellation_token.is_cancelled() {
            return Err(mai_model::ModelError::Cancelled);
        }
        let event = event?;
        accumulator.push(&event);
    }
    let response = accumulator.finish()?;
    model.apply_completed_state(state, response.id.as_deref());
    Ok(response)
}

async fn consume_turn_stream(
    model: &ModelClient,
    store: &ConfigStore,
    events: &RuntimeEvents,
    agent: &Arc<AgentRecord>,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    turn_model_state: &mut ModelTurnState,
    mut stream: ModelEventStream,
    cancellation_token: &CancellationToken,
) -> Result<ModelTurnResult> {
    let message_id = format!("msg_{}", Uuid::new_v4());
    let reasoning_id = format!("reasoning_{}", Uuid::new_v4());
    let mut response_id = None;
    let mut usage = None;
    let mut text_parts: HashMap<usize, String> = HashMap::new();
    let mut reasoning_parts: HashMap<usize, String> = HashMap::new();
    let mut tool_previews: HashMap<usize, StreamToolPreview> = HashMap::new();
    let mut output_items = Vec::new();
    let mut tool_calls = Vec::new();
    let mut made_progress = false;
    let mut final_text = String::new();
    let mut final_reasoning = String::new();
    while let Some(event) = stream.next().await {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        let event = event.map_err(model_error_to_runtime)?;
        match &event {
            ModelStreamEvent::ResponseStarted { id } => {
                if response_id.is_none() {
                    response_id = id.clone();
                }
            }
            ModelStreamEvent::TextDelta {
                output_index,
                delta,
                ..
            } if !delta.is_empty() => {
                text_parts.entry(*output_index).or_default().push_str(delta);
                final_text.push_str(delta);
                events
                    .publish(ServiceEventKind::AgentMessageDelta {
                        agent_id,
                        session_id: Some(session_id),
                        turn_id,
                        message_id: message_id.clone(),
                        role: MessageRole::Assistant,
                        channel: "final".to_string(),
                        delta: delta.clone(),
                    })
                    .await;
            }
            ModelStreamEvent::ReasoningDelta {
                output_index,
                delta,
                ..
            } if !delta.is_empty() => {
                reasoning_parts
                    .entry(*output_index)
                    .or_default()
                    .push_str(delta);
                final_reasoning.push_str(delta);
                events
                    .publish(ServiceEventKind::ReasoningDelta {
                        agent_id,
                        session_id: Some(session_id),
                        turn_id,
                        message_id: reasoning_id.clone(),
                        delta: delta.clone(),
                    })
                    .await;
            }
            ModelStreamEvent::OutputItemAdded { output_index, item } => {
                if let ModelOutputItem::FunctionCall { call_id, name, .. } = item {
                    let preview = tool_previews.entry(*output_index).or_default();
                    if !call_id.is_empty() {
                        preview.call_id = Some(call_id.clone());
                    }
                    if !name.is_empty() {
                        preview.name = Some(name.clone());
                    }
                }
            }
            ModelStreamEvent::ToolCallStarted {
                call_id,
                name,
                output_index,
            } => {
                let preview = tool_previews.entry(*output_index).or_default();
                if call_id.is_some() {
                    preview.call_id = call_id.clone();
                }
                if name.is_some() {
                    preview.name = name.clone();
                }
            }
            ModelStreamEvent::ToolCallArgumentsDelta {
                output_index,
                delta,
            } if !delta.is_empty() => {
                let preview = tool_previews.entry(*output_index).or_default();
                preview.arguments.push_str(delta);
                if let (Some(call_id), Some(name)) = (preview.call_id.clone(), preview.name.clone())
                {
                    events
                        .publish(ServiceEventKind::ToolCallDelta {
                            agent_id,
                            session_id: Some(session_id),
                            turn_id,
                            call_id,
                            tool_name: name,
                            arguments_delta: delta.clone(),
                        })
                        .await;
                }
            }
            ModelStreamEvent::OutputItemDone { output_index, item } => {
                output_items.push(item.clone());
                match item.clone() {
                    ModelOutputItem::Message { text } => {
                        let text = if text.trim().is_empty() {
                            text_parts.remove(output_index).unwrap_or_default()
                        } else {
                            text
                        };
                        let reasoning_content = reasoning_parts
                            .remove(output_index)
                            .filter(|reasoning| !reasoning.trim().is_empty());
                        if !text.trim().is_empty() {
                            made_progress = true;
                            final_text = text.clone();
                            if let Some(value) = &reasoning_content {
                                final_reasoning = value.clone();
                            }
                            super::history::record_message(
                                store,
                                agent,
                                agent_id,
                                session_id,
                                MessageRole::Assistant,
                                text.clone(),
                            )
                            .await?;
                            super::history::record_history_item(
                                store,
                                agent,
                                agent_id,
                                session_id,
                                if reasoning_content.is_some() {
                                    ModelInputItem::AssistantTurn {
                                        content: Some(text.clone()),
                                        reasoning_content: reasoning_content.clone(),
                                        tool_calls: Vec::new(),
                                    }
                                } else {
                                    ModelInputItem::assistant_text(text.clone())
                                },
                            )
                            .await?;
                            events
                                .publish(ServiceEventKind::AgentMessageCompleted {
                                    agent_id,
                                    session_id: Some(session_id),
                                    turn_id,
                                    message_id: message_id.clone(),
                                    role: MessageRole::Assistant,
                                    channel: "final".to_string(),
                                    content: text.clone(),
                                })
                                .await;
                            events
                                .publish(ServiceEventKind::AgentMessage {
                                    agent_id,
                                    session_id: Some(session_id),
                                    turn_id: Some(turn_id),
                                    role: MessageRole::Assistant,
                                    content: text,
                                })
                                .await;
                        }
                    }
                    ModelOutputItem::FunctionCall {
                        call_id,
                        name,
                        arguments,
                        raw_arguments,
                    } => {
                        made_progress = true;
                        let call_id = if call_id.is_empty() {
                            format!("call_{}", Uuid::new_v4())
                        } else {
                            call_id
                        };
                        super::history::record_history_item(
                            store,
                            agent,
                            agent_id,
                            session_id,
                            ModelInputItem::FunctionCall {
                                call_id: call_id.clone(),
                                name: name.clone(),
                                arguments: raw_arguments,
                            },
                        )
                        .await?;
                        tool_calls.push((call_id, name, arguments));
                    }
                    ModelOutputItem::AssistantTurn {
                        content,
                        reasoning_content,
                        tool_calls: output_tool_calls,
                    } => {
                        let assistant_tool_calls = output_tool_calls
                            .into_iter()
                            .map(|tool_call| {
                                let call_id = if tool_call.call_id.is_empty() {
                                    format!("call_{}", Uuid::new_v4())
                                } else {
                                    tool_call.call_id
                                };
                                let name = tool_call.name;
                                let arguments = tool_call.arguments;
                                let raw_arguments = tool_call.raw_arguments;
                                tool_calls.push((call_id.clone(), name.clone(), arguments));
                                ModelToolCall {
                                    call_id,
                                    name,
                                    arguments: raw_arguments,
                                }
                            })
                            .collect::<Vec<_>>();
                        let content = content
                            .or_else(|| text_parts.remove(output_index))
                            .filter(|text| !text.trim().is_empty());
                        let reasoning_content = reasoning_content
                            .or_else(|| reasoning_parts.remove(output_index))
                            .filter(|reasoning| !reasoning.trim().is_empty());
                        let has_content = content.is_some();
                        let has_reasoning = reasoning_content.is_some();
                        if has_content || has_reasoning || !assistant_tool_calls.is_empty() {
                            made_progress = true;
                            if let Some(value) = &reasoning_content {
                                final_reasoning = value.clone();
                            }
                            super::history::record_history_item(
                                store,
                                agent,
                                agent_id,
                                session_id,
                                ModelInputItem::AssistantTurn {
                                    content: content.clone(),
                                    reasoning_content: reasoning_content.clone(),
                                    tool_calls: assistant_tool_calls,
                                },
                            )
                            .await?;
                        }
                        if let Some(text) = content {
                            final_text = text.clone();
                            super::history::record_message(
                                store,
                                agent,
                                agent_id,
                                session_id,
                                MessageRole::Assistant,
                                text.clone(),
                            )
                            .await?;
                            events
                                .publish(ServiceEventKind::AgentMessageCompleted {
                                    agent_id,
                                    session_id: Some(session_id),
                                    turn_id,
                                    message_id: message_id.clone(),
                                    role: MessageRole::Assistant,
                                    channel: "final".to_string(),
                                    content: text.clone(),
                                })
                                .await;
                            events
                                .publish(ServiceEventKind::AgentMessage {
                                    agent_id,
                                    session_id: Some(session_id),
                                    turn_id: Some(turn_id),
                                    role: MessageRole::Assistant,
                                    content: text,
                                })
                                .await;
                        }
                    }
                    ModelOutputItem::Other { .. } => {}
                }
            }
            ModelStreamEvent::Completed {
                id,
                usage: done_usage,
                ..
            } => {
                if let Some(id) = id {
                    response_id = Some(id.clone());
                }
                usage = done_usage.clone();
            }
            _ => {}
        }
    }
    if !final_text.trim().is_empty() && output_items.is_empty() {
        made_progress = true;
        let text = final_text.clone();
        output_items.push(ModelOutputItem::Message { text: text.clone() });
        super::history::record_message(
            store,
            agent,
            agent_id,
            session_id,
            MessageRole::Assistant,
            text.clone(),
        )
        .await?;
        super::history::record_history_item(
            store,
            agent,
            agent_id,
            session_id,
            ModelInputItem::assistant_text(text.clone()),
        )
        .await?;
        events
            .publish(ServiceEventKind::AgentMessageCompleted {
                agent_id,
                session_id: Some(session_id),
                turn_id,
                message_id: message_id.clone(),
                role: MessageRole::Assistant,
                channel: "final".to_string(),
                content: text.clone(),
            })
            .await;
        events
            .publish(ServiceEventKind::AgentMessage {
                agent_id,
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                role: MessageRole::Assistant,
                content: text,
            })
            .await;
    }
    if final_text.trim().is_empty() && !final_reasoning.trim().is_empty() && output_items.is_empty()
    {
        made_progress = true;
        output_items.push(ModelOutputItem::AssistantTurn {
            content: None,
            reasoning_content: Some(final_reasoning.clone()),
            tool_calls: Vec::new(),
        });
        super::history::record_history_item(
            store,
            agent,
            agent_id,
            session_id,
            ModelInputItem::AssistantTurn {
                content: None,
                reasoning_content: Some(final_reasoning.clone()),
                tool_calls: Vec::new(),
            },
        )
        .await?;
    }
    if !final_reasoning.trim().is_empty() {
        events
            .publish(ServiceEventKind::ReasoningCompleted {
                agent_id,
                session_id: Some(session_id),
                turn_id,
                message_id: reasoning_id,
                content: final_reasoning,
            })
            .await;
    }
    model.apply_completed_state(turn_model_state, response_id.as_deref());
    let response = ModelResponse {
        id: response_id,
        output: output_items,
        usage,
    };
    Ok(ModelTurnResult {
        response,
        tool_calls,
        last_assistant_text: (!final_text.trim().is_empty()).then_some(final_text),
        made_progress,
    })
}

pub(crate) fn model_error_to_runtime(err: mai_model::ModelError) -> RuntimeError {
    if matches!(err, mai_model::ModelError::Cancelled) {
        RuntimeError::TurnCancelled
    } else {
        RuntimeError::Model(err)
    }
}
