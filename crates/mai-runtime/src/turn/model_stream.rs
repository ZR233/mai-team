use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::StreamExt;
use mai_protocol::{
    AgentId, MessageRole, ModelOutputItem, ModelResponse, ServiceEventKind, SessionId, TokenUsage,
    ToolDefinition, TurnId,
};
use mai_store::{ConfigStore, ProviderSelection};
use pl_model::{
    CompletionBlockContent, CompletionBlockField, CompletionBlockKind, CompletionEventStream,
    CompletionStreamAccumulator, CompletionStreamEvent, ModelProvider, ToolInputDeltaPayload,
};
use pl_protocol::{Message, PureError};
use pl_trace::TraceTextChannel;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::events::RuntimeEvents;
use crate::state::AgentRecord;
use crate::{ModelClient, ModelTurnState};
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
struct ActiveItem {
    text: String,
    reasoning: String,
}

#[derive(Debug, Clone, Default)]
struct ToolPreview {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    custom_input: Option<String>,
}

struct ModelStreamReducer<'a> {
    store: &'a ConfigStore,
    events: &'a RuntimeEvents,
    agent: &'a Arc<AgentRecord>,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    message_id: String,
    reasoning_id: String,
    active_items: HashMap<usize, ActiveItem>,
    stream_indexes: HashMap<String, usize>,
    next_stream_index: usize,
    completed_output_indices: HashSet<usize>,
    final_output_items: Vec<ModelOutputItem>,
    pending_tool_previews: HashMap<usize, ToolPreview>,
    response_id: Option<String>,
    usage: Option<TokenUsage>,
    completed: bool,
    last_assistant_text: Option<String>,
    fallback_text: String,
    fallback_reasoning: String,
    tool_calls: Vec<(String, String, Value)>,
    made_progress: bool,
}

impl<'a> ModelStreamReducer<'a> {
    fn new(
        store: &'a ConfigStore,
        events: &'a RuntimeEvents,
        agent: &'a Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Self {
        Self {
            store,
            events,
            agent,
            agent_id,
            session_id,
            turn_id,
            message_id: format!("msg_{}", Uuid::new_v4()),
            reasoning_id: format!("reasoning_{}", Uuid::new_v4()),
            active_items: HashMap::new(),
            stream_indexes: HashMap::new(),
            next_stream_index: 0,
            completed_output_indices: HashSet::new(),
            final_output_items: Vec::new(),
            pending_tool_previews: HashMap::new(),
            response_id: None,
            usage: None,
            completed: false,
            last_assistant_text: None,
            fallback_text: String::new(),
            fallback_reasoning: String::new(),
            tool_calls: Vec::new(),
            made_progress: false,
        }
    }

    async fn handle_event(&mut self, event: CompletionStreamEvent) -> Result<()> {
        match event {
            CompletionStreamEvent::ResponseStarted { response_id } => {
                if self.response_id.is_none() {
                    self.response_id = response_id;
                }
            }
            CompletionStreamEvent::BlockOpened { id, kind, .. } => {
                let output_index = self.output_index(&id);
                if self.completed_output_indices.contains(&output_index) {
                    return Ok(());
                }
                if matches!(
                    kind,
                    CompletionBlockKind::Text { .. } | CompletionBlockKind::ReasoningSummary
                ) {
                    self.active_items.entry(output_index).or_default();
                }
            }
            CompletionStreamEvent::BlockDelta {
                id,
                kind: CompletionBlockKind::Text { channel },
                field: CompletionBlockField::Text,
                delta,
                ..
            } => {
                let output_index = self.output_index(&id);
                if delta.is_empty() || self.completed_output_indices.contains(&output_index) {
                    return Ok(());
                }
                self.active_items
                    .entry(output_index)
                    .or_default()
                    .text
                    .push_str(&delta);
                if channel == TraceTextChannel::Final {
                    self.fallback_text.push_str(&delta);
                    self.events
                        .publish(ServiceEventKind::AgentMessageDelta {
                            agent_id: self.agent_id,
                            session_id: Some(self.session_id),
                            turn_id: self.turn_id,
                            message_id: self.message_id.clone(),
                            role: MessageRole::Assistant,
                            channel: "final".to_string(),
                            delta,
                        })
                        .await;
                }
            }
            CompletionStreamEvent::BlockDelta {
                id,
                kind: CompletionBlockKind::ReasoningSummary,
                field: CompletionBlockField::ReasoningSummary,
                delta,
                ..
            } => {
                let output_index = self.output_index(&id);
                if delta.is_empty() || self.completed_output_indices.contains(&output_index) {
                    return Ok(());
                }
                self.active_items
                    .entry(output_index)
                    .or_default()
                    .reasoning
                    .push_str(&delta);
                self.fallback_reasoning.push_str(&delta);
                self.events
                    .publish(ServiceEventKind::ReasoningDelta {
                        agent_id: self.agent_id,
                        session_id: Some(self.session_id),
                        turn_id: self.turn_id,
                        message_id: self.reasoning_id.clone(),
                        delta,
                    })
                    .await;
            }
            CompletionStreamEvent::ReasoningRawDelta { id, delta, .. } => {
                let output_index = self.output_index(&id);
                if delta.is_empty() || self.completed_output_indices.contains(&output_index) {
                    return Ok(());
                }
                self.active_items
                    .entry(output_index)
                    .or_default()
                    .reasoning
                    .push_str(&delta);
                self.fallback_reasoning.push_str(&delta);
                self.events
                    .publish(ServiceEventKind::ReasoningDelta {
                        agent_id: self.agent_id,
                        session_id: Some(self.session_id),
                        turn_id: self.turn_id,
                        message_id: self.reasoning_id.clone(),
                        delta,
                    })
                    .await;
            }
            CompletionStreamEvent::ToolInputStarted {
                item_id,
                call_id,
                name,
                payload_kind: _,
                ..
            } => {
                let output_index = self.output_index(&item_id);
                if self.completed_output_indices.contains(&output_index) {
                    return Ok(());
                }
                self.active_items.entry(output_index).or_default();
                self.update_tool_preview(output_index, call_id, name);
            }
            CompletionStreamEvent::ToolInputDelta {
                item_id,
                call_id,
                name,
                payload_delta,
                ..
            } => {
                let output_index = self.output_index(&item_id);
                self.update_tool_preview(output_index, call_id, name);
                let delta = payload_delta.text().to_string();
                if delta.is_empty() || self.completed_output_indices.contains(&output_index) {
                    return Ok(());
                }
                let preview = {
                    let preview = self.pending_tool_previews.entry(output_index).or_default();
                    match payload_delta {
                        ToolInputDeltaPayload::FunctionArguments(_) => {
                            preview.arguments.push_str(&delta);
                        }
                        ToolInputDeltaPayload::CustomInput(_) => {
                            preview
                                .custom_input
                                .get_or_insert_with(String::new)
                                .push_str(&delta);
                        }
                    }
                    preview.call_id.clone().zip(preview.name.clone())
                };
                if let Some((call_id, name)) = preview {
                    self.events
                        .publish(ServiceEventKind::ToolCallDelta {
                            agent_id: self.agent_id,
                            session_id: Some(self.session_id),
                            turn_id: self.turn_id,
                            call_id,
                            tool_name: name,
                            arguments_delta: delta,
                        })
                        .await;
                }
            }
            CompletionStreamEvent::BlockClosed {
                id,
                kind: CompletionBlockKind::Text { channel },
                authoritative_content,
                ..
            } => {
                let output_index = self.output_index(&id);
                if !self.completed_output_indices.insert(output_index) {
                    return Ok(());
                }
                let text = match authoritative_content {
                    Some(CompletionBlockContent::Text(text)) => text,
                    Some(
                        CompletionBlockContent::ReasoningSummary(_)
                        | CompletionBlockContent::Plan(_),
                    )
                    | None => self
                        .active_items
                        .get(&output_index)
                        .map(|active| active.text.clone())
                        .unwrap_or_default(),
                };
                if channel == TraceTextChannel::Final {
                    self.finalize_output_item(output_index, ModelOutputItem::Message { text })
                        .await?;
                }
            }
            CompletionStreamEvent::BlockClosed {
                id,
                kind: CompletionBlockKind::ReasoningSummary,
                authoritative_content,
                ..
            } => {
                let output_index = self.output_index(&id);
                if !self.completed_output_indices.insert(output_index) {
                    return Ok(());
                }
                let content = match authoritative_content {
                    Some(CompletionBlockContent::ReasoningSummary(parts)) => parts.join(""),
                    Some(
                        CompletionBlockContent::Text(text) | CompletionBlockContent::Plan(text),
                    ) => text,
                    None => self
                        .active_items
                        .get(&output_index)
                        .map(|active| active.reasoning.clone())
                        .unwrap_or_default(),
                };
                self.finalize_output_item(output_index, ModelOutputItem::Reasoning { content })
                    .await?;
            }
            CompletionStreamEvent::ToolInputCompleted {
                item_id,
                call_id,
                name,
                payload,
                ..
            }
            | CompletionStreamEvent::ToolCallReady {
                item_id,
                call_id,
                name,
                payload,
                ..
            } => {
                let output_index = self.output_index(&item_id);
                if !self.completed_output_indices.insert(output_index) {
                    return Ok(());
                }
                self.update_tool_preview(output_index, call_id, name);
                if let Some(payload) = payload {
                    let preview = self.pending_tool_previews.entry(output_index).or_default();
                    match payload {
                        ToolInputDeltaPayload::FunctionArguments(arguments) => {
                            preview.arguments = arguments;
                            preview.custom_input = None;
                        }
                        ToolInputDeltaPayload::CustomInput(input) => {
                            preview.custom_input = Some(input);
                            preview.arguments.clear();
                        }
                    }
                }
                let item = self.tool_preview_output_item(output_index);
                self.finalize_output_item(output_index, item).await?;
            }
            CompletionStreamEvent::Usage(usage) => {
                self.usage = Some(TokenUsage {
                    input_tokens: usage.prompt_tokens,
                    cached_input_tokens: usage.cached_prompt_tokens,
                    output_tokens: usage.completion_tokens,
                    reasoning_output_tokens: usage.reasoning_tokens,
                    total_tokens: usage.total_tokens,
                });
            }
            CompletionStreamEvent::Completed { response_id } => {
                self.completed = true;
                if let Some(response_id) = response_id {
                    self.response_id = Some(response_id);
                }
            }
            CompletionStreamEvent::Failed { message, .. } => {
                return Err(PureError::LlmError(message).into());
            }
            CompletionStreamEvent::BlockDelta { .. }
            | CompletionStreamEvent::BlockClosed { .. } => {}
        }
        Ok(())
    }

    fn output_index(&mut self, id: &str) -> usize {
        if let Some(index) = self.stream_indexes.get(id) {
            return *index;
        }
        let index = self.next_stream_index;
        self.next_stream_index += 1;
        self.stream_indexes.insert(id.to_string(), index);
        index
    }

    fn tool_preview_output_item(&self, output_index: usize) -> ModelOutputItem {
        let preview = self
            .pending_tool_previews
            .get(&output_index)
            .cloned()
            .unwrap_or_default();
        let raw_arguments = preview
            .custom_input
            .clone()
            .unwrap_or_else(|| preview.arguments.clone());
        let arguments = preview
            .custom_input
            .map(|input| serde_json::json!({ "input": input, "patch": input }))
            .unwrap_or_else(|| parse_arguments(&raw_arguments));
        ModelOutputItem::FunctionCall {
            call_id: normalized_call_id(preview.call_id.unwrap_or_default()),
            name: preview.name.unwrap_or_default(),
            arguments,
            raw_arguments,
        }
    }

    fn update_tool_preview(
        &mut self,
        output_index: usize,
        call_id: Option<String>,
        name: Option<String>,
    ) {
        let preview = self.pending_tool_previews.entry(output_index).or_default();
        if let Some(call_id) = call_id.filter(|value| !value.is_empty()) {
            preview.call_id = Some(call_id);
        }
        if let Some(name) = name.filter(|value| !value.is_empty()) {
            preview.name = Some(name);
        }
    }

    async fn finalize_output_item(
        &mut self,
        output_index: usize,
        item: ModelOutputItem,
    ) -> Result<()> {
        let active = self.active_items.remove(&output_index).unwrap_or_default();
        self.pending_tool_previews.remove(&output_index);
        match item {
            ModelOutputItem::Message { text } => {
                let text = if text.trim().is_empty() {
                    active.text
                } else {
                    text
                };
                if !active.reasoning.trim().is_empty() {
                    self.record_reasoning_item(active.reasoning).await?;
                }
                if !text.trim().is_empty() {
                    self.made_progress = true;
                    self.final_output_items
                        .push(ModelOutputItem::Message { text: text.clone() });
                    self.record_assistant_message(text.clone()).await?;
                    super::history::record_history_item(
                        self.store,
                        self.agent,
                        self.agent_id,
                        self.session_id,
                        super::history::assistant_text_message(text),
                    )
                    .await?;
                }
            }
            ModelOutputItem::Reasoning { content } => {
                let content = if content.trim().is_empty() {
                    active.reasoning
                } else {
                    content
                };
                self.record_reasoning_item(content).await?;
            }
            ModelOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                raw_arguments,
            } => {
                self.made_progress = true;
                let call_id = normalized_call_id(call_id);
                if !active.reasoning.trim().is_empty() {
                    self.record_reasoning_item(active.reasoning).await?;
                }
                if !active.text.trim().is_empty() {
                    let text = active.text;
                    self.final_output_items
                        .push(ModelOutputItem::Message { text: text.clone() });
                    self.record_assistant_message(text.clone()).await?;
                    super::history::record_history_item(
                        self.store,
                        self.agent,
                        self.agent_id,
                        self.session_id,
                        super::history::assistant_text_message(text),
                    )
                    .await?;
                }
                self.final_output_items.push(ModelOutputItem::FunctionCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    raw_arguments: raw_arguments.clone(),
                });
                super::history::record_history_item(
                    self.store,
                    self.agent,
                    self.agent_id,
                    self.session_id,
                    super::history::tool_call_message(call_id.clone(), name.clone(), raw_arguments),
                )
                .await?;
                self.tool_calls.push((call_id, name, arguments));
            }
            ModelOutputItem::Other { raw } => {
                self.final_output_items.push(ModelOutputItem::Other { raw });
            }
        }
        Ok(())
    }

    async fn finish(mut self) -> Result<ModelTurnResult> {
        if !self.completed {
            return Err(
                PureError::LlmError("stream closed before response.completed".to_string()).into(),
            );
        }
        self.persist_legacy_delta_fallback().await?;
        self.finalize_pending_tool_calls().await?;
        Ok(ModelTurnResult {
            response: ModelResponse {
                id: self.response_id,
                output: self.final_output_items,
                usage: self.usage,
            },
            tool_calls: self.tool_calls,
            last_assistant_text: self.last_assistant_text,
            made_progress: self.made_progress,
        })
    }

    async fn persist_legacy_delta_fallback(&mut self) -> Result<()> {
        if !self.final_output_items.is_empty() {
            return Ok(());
        }
        let has_text = !self.fallback_text.trim().is_empty();
        let has_reasoning = !self.fallback_reasoning.trim().is_empty();
        if has_text && has_reasoning {
            self.made_progress = true;
            let text = self.fallback_text.clone();
            let reasoning = self.fallback_reasoning.clone();
            self.record_reasoning_item(reasoning).await?;
            self.final_output_items
                .push(ModelOutputItem::Message { text: text.clone() });
            self.record_assistant_message(text.clone()).await?;
            super::history::record_history_item(
                self.store,
                self.agent,
                self.agent_id,
                self.session_id,
                super::history::assistant_text_message(text),
            )
            .await?;
        } else if has_text {
            self.made_progress = true;
            let text = self.fallback_text.clone();
            self.final_output_items
                .push(ModelOutputItem::Message { text: text.clone() });
            self.record_assistant_message(text.clone()).await?;
            super::history::record_history_item(
                self.store,
                self.agent,
                self.agent_id,
                self.session_id,
                super::history::assistant_text_message(text),
            )
            .await?;
        } else if has_reasoning {
            self.made_progress = true;
            let reasoning = self.fallback_reasoning.clone();
            self.record_reasoning_item(reasoning).await?;
        }
        Ok(())
    }

    async fn finalize_pending_tool_calls(&mut self) -> Result<()> {
        if self.pending_tool_previews.is_empty() {
            return Ok(());
        }
        let pending_indices: Vec<usize> = self.pending_tool_previews.keys().copied().collect();
        for index in pending_indices {
            let active = self.active_items.remove(&index).unwrap_or_default();
            let preview = match self.pending_tool_previews.remove(&index) {
                Some(preview) => preview,
                None => continue,
            };
            let call_id = normalized_call_id(preview.call_id.unwrap_or_default());
            let name = preview.name.unwrap_or_default();
            let raw_arguments = preview.arguments;
            let arguments = parse_arguments(&raw_arguments);
            if !active.reasoning.trim().is_empty() {
                self.record_reasoning_item(active.reasoning).await?;
            }
            if !active.text.trim().is_empty() {
                let text = active.text;
                self.final_output_items
                    .push(ModelOutputItem::Message { text: text.clone() });
                self.record_assistant_message(text.clone()).await?;
                super::history::record_history_item(
                    self.store,
                    self.agent,
                    self.agent_id,
                    self.session_id,
                    super::history::assistant_text_message(text),
                )
                .await?;
            }
            self.made_progress = true;
            self.final_output_items.push(ModelOutputItem::FunctionCall {
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
                raw_arguments: raw_arguments.clone(),
            });
            super::history::record_history_item(
                self.store,
                self.agent,
                self.agent_id,
                self.session_id,
                super::history::tool_call_message(call_id.clone(), name.clone(), raw_arguments),
            )
            .await?;
            self.tool_calls.push((call_id, name, arguments));
        }
        Ok(())
    }

    async fn record_reasoning_item(&mut self, content: String) -> Result<()> {
        if content.trim().is_empty() {
            return Ok(());
        }
        self.made_progress = true;
        self.final_output_items.push(ModelOutputItem::Reasoning {
            content: content.clone(),
        });
        super::history::record_history_item(
            self.store,
            self.agent,
            self.agent_id,
            self.session_id,
            super::history::reasoning_message(content.clone()),
        )
        .await?;
        self.events
            .publish(ServiceEventKind::ReasoningCompleted {
                agent_id: self.agent_id,
                session_id: Some(self.session_id),
                turn_id: self.turn_id,
                message_id: self.reasoning_id.clone(),
                content,
            })
            .await;
        Ok(())
    }

    async fn record_assistant_message(&mut self, text: String) -> Result<()> {
        self.last_assistant_text = Some(text.clone());
        super::history::record_message(
            self.store,
            self.agent,
            self.agent_id,
            self.session_id,
            MessageRole::Assistant,
            text.clone(),
        )
        .await?;
        self.events
            .publish(ServiceEventKind::AgentMessageCompleted {
                agent_id: self.agent_id,
                session_id: Some(self.session_id),
                turn_id: self.turn_id,
                message_id: self.message_id.clone(),
                role: MessageRole::Assistant,
                channel: "final".to_string(),
                content: text.clone(),
            })
            .await;
        self.events
            .publish(ServiceEventKind::AgentMessage {
                agent_id: self.agent_id,
                session_id: Some(self.session_id),
                turn_id: Some(self.turn_id),
                role: MessageRole::Assistant,
                content: text,
            })
            .await;
        Ok(())
    }
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
    history: &[Message],
    turn_model_state: &mut ModelTurnState,
    cancellation_token: &CancellationToken,
) -> Result<ModelTurnResult> {
    turn_model_state.prompt_cache_key =
        Some(format!("agent:{}:session:{}", ctx.agent_id, ctx.session_id));
    let mut prepared = model
        .prepare_turn(
            &model_context.provider_selection,
            model_context.reasoning_effort.as_deref(),
            model_context.instructions.clone(),
            history.to_vec(),
            &model_context.tools,
            turn_model_state,
        )
        .await?;
    let had_continuation = prepared.request.previous_response_id.is_some();
    let stream = match prepared.provider.stream_events(prepared.request).await {
        Ok(stream) => stream,
        Err(err) if had_continuation && ModelClient::is_continuation_unsupported_error(&err) => {
            model
                .mark_continuation_unsupported(&model_context.provider_selection, turn_model_state)
                .await;
            prepared = model
                .prepare_turn(
                    &model_context.provider_selection,
                    model_context.reasoning_effort.as_deref(),
                    model_context.instructions.clone(),
                    history.to_vec(),
                    &model_context.tools,
                    turn_model_state,
                )
                .await?;
            prepared.provider.stream_events(prepared.request).await?
        }
        Err(err) => return Err(err.into()),
    };
    consume_turn_stream(
        model,
        ctx,
        turn_model_state,
        history.len(),
        stream,
        cancellation_token,
    )
    .await
}

pub(crate) async fn consume_model_stream_to_response(
    model: &ModelClient,
    provider_selection: &ProviderSelection,
    instructions: &str,
    input: &[Message],
    tools: &[ToolDefinition],
    state: &mut ModelTurnState,
    cancellation_token: &CancellationToken,
) -> std::result::Result<ModelResponse, PureError> {
    let mut prepared = model
        .prepare_turn(
            provider_selection,
            None,
            instructions.to_string(),
            input.to_vec(),
            tools,
            state,
        )
        .await?;
    let had_continuation = prepared.request.previous_response_id.is_some();
    let mut stream = match prepared.provider.stream_events(prepared.request).await {
        Ok(stream) => stream,
        Err(err) if had_continuation && ModelClient::is_continuation_unsupported_error(&err) => {
            model
                .mark_continuation_unsupported(provider_selection, state)
                .await;
            prepared = model
                .prepare_turn(
                    provider_selection,
                    None,
                    instructions.to_string(),
                    input.to_vec(),
                    tools,
                    state,
                )
                .await?;
            prepared.provider.stream_events(prepared.request).await?
        }
        Err(err) => return Err(err),
    };
    let mut accumulator = CompletionStreamAccumulator::new(None);
    let (event_tx, _event_rx) = tokio::sync::broadcast::channel(1);
    while let Some(event) = stream.next().await {
        if cancellation_token.is_cancelled() {
            return Err(PureError::LlmError("request cancelled".to_string()));
        }
        let event = event?;
        accumulator.apply(event, &event_tx)?;
    }
    let response = accumulator.finish(&event_tx)?;
    model.apply_completed_state(state, input.len(), response_id(&response).as_deref());
    Ok(model_response(response))
}

async fn consume_turn_stream(
    model: &ModelClient,
    ctx: &TurnStreamContext<'_>,
    turn_model_state: &mut ModelTurnState,
    history_len: usize,
    mut stream: CompletionEventStream,
    cancellation_token: &CancellationToken,
) -> Result<ModelTurnResult> {
    let mut reducer = ModelStreamReducer::new(
        ctx.store,
        ctx.events,
        ctx.agent,
        ctx.agent_id,
        ctx.session_id,
        ctx.turn_id,
    );
    while let Some(event) = stream.next().await {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        let event = event?;
        reducer.handle_event(event).await?;
    }
    let result = reducer.finish().await?;
    model.apply_completed_state(turn_model_state, history_len, result.response.id.as_deref());
    Ok(result)
}

fn normalized_call_id(call_id: String) -> String {
    if call_id.is_empty() {
        format!("call_{}", Uuid::new_v4())
    } else {
        call_id
    }
}

fn parse_arguments(raw_arguments: &str) -> Value {
    serde_json::from_str(raw_arguments)
        .unwrap_or_else(|_| serde_json::json!({ "raw": raw_arguments }))
}

fn response_id(response: &pl_model::CompletionResponse) -> Option<String> {
    match response.finish_reason {
        pl_model::FinishReason::Stop
        | pl_model::FinishReason::ToolCalls
        | pl_model::FinishReason::MaxTokens
        | pl_model::FinishReason::ContentFilter => None,
    }
}

fn model_response(response: pl_model::CompletionResponse) -> ModelResponse {
    let mut output = Vec::new();
    if let Some(reasoning) = response
        .reasoning_content
        .filter(|text| !text.trim().is_empty())
    {
        output.push(ModelOutputItem::Reasoning { content: reasoning });
    }
    if let Some(content) = response.content.filter(|text| !text.trim().is_empty()) {
        output.push(ModelOutputItem::Message { text: content });
    }
    output.extend(response.tool_calls.into_iter().map(|call| {
        let raw_arguments = call.payload_text();
        let arguments = call.arguments_for_tool();
        let call_id = call.call_id.clone().unwrap_or_else(|| call.id.clone());
        ModelOutputItem::FunctionCall {
            call_id,
            name: call.name,
            arguments,
            raw_arguments,
        }
    }));
    ModelResponse {
        id: None,
        output,
        usage: Some(TokenUsage {
            input_tokens: response.usage.prompt_tokens,
            cached_input_tokens: response.usage.cached_prompt_tokens,
            output_tokens: response.usage.completion_tokens,
            reasoning_output_tokens: response.usage.reasoning_tokens,
            total_tokens: response.usage.total_tokens,
        }),
    }
}
