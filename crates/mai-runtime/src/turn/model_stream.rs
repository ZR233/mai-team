use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use mai_model::{
    ModelClient, ModelEventStream, ModelStreamAccumulator, ModelStreamEvent, ModelTurnState,
};
use mai_protocol::{
    AgentId, MessageRole, ModelInputItem, ModelOutputItem, ModelResponse, ModelToolCall,
    ServiceEventKind, SessionId, TokenUsage, ToolDefinition, TurnId,
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
struct ActiveItem {
    text: String,
    reasoning: String,
}

#[derive(Debug, Clone, Default)]
struct ToolPreview {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
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
    final_output_items: Vec<ModelOutputItem>,
    pending_tool_previews: HashMap<usize, ToolPreview>,
    response_id: Option<String>,
    usage: Option<TokenUsage>,
    completed: bool,
    last_assistant_text: Option<String>,
    last_reasoning_content: Option<String>,
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
            final_output_items: Vec::new(),
            pending_tool_previews: HashMap::new(),
            response_id: None,
            usage: None,
            completed: false,
            last_assistant_text: None,
            last_reasoning_content: None,
            fallback_text: String::new(),
            fallback_reasoning: String::new(),
            tool_calls: Vec::new(),
            made_progress: false,
        }
    }

    async fn handle_event(&mut self, event: ModelStreamEvent) -> Result<()> {
        match event {
            ModelStreamEvent::ResponseStarted { id } => {
                if self.response_id.is_none() {
                    self.response_id = id;
                }
            }
            ModelStreamEvent::OutputItemAdded { output_index, item } => {
                self.active_items.entry(output_index).or_default();
                if let ModelOutputItem::FunctionCall { call_id, name, .. } = item {
                    self.update_tool_preview(output_index, Some(call_id), Some(name));
                }
            }
            ModelStreamEvent::TextDelta {
                output_index,
                delta,
                ..
            } => {
                if delta.is_empty() {
                    return Ok(());
                }
                self.active_items
                    .entry(output_index)
                    .or_default()
                    .text
                    .push_str(&delta);
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
            ModelStreamEvent::ReasoningDelta {
                output_index,
                delta,
                ..
            } => {
                if delta.is_empty() {
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
            ModelStreamEvent::ToolCallStarted {
                output_index,
                call_id,
                name,
            } => {
                self.active_items.entry(output_index).or_default();
                self.update_tool_preview(output_index, call_id, name);
            }
            ModelStreamEvent::ToolCallArgumentsDelta {
                output_index,
                delta,
            } => {
                if delta.is_empty() {
                    return Ok(());
                }
                let preview = {
                    let preview = self.pending_tool_previews.entry(output_index).or_default();
                    preview.arguments.push_str(&delta);
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
            ModelStreamEvent::OutputItemDone { output_index, item } => {
                self.finalize_output_item(output_index, item).await?;
            }
            ModelStreamEvent::Completed { id, usage, .. } => {
                self.completed = true;
                if let Some(id) = id {
                    self.response_id = Some(id);
                }
                if let Some(usage) = usage {
                    self.usage = Some(usage);
                }
            }
            ModelStreamEvent::Status { .. } | ModelStreamEvent::CustomToolInputDelta { .. } => {}
        }
        Ok(())
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
                let reasoning_content =
                    (!active.reasoning.trim().is_empty()).then_some(active.reasoning);
                if reasoning_content.is_some() {
                    self.final_output_items
                        .push(ModelOutputItem::AssistantTurn {
                            content: (!text.trim().is_empty()).then_some(text.clone()),
                            reasoning_content: reasoning_content.clone(),
                            tool_calls: Vec::new(),
                        });
                } else {
                    self.final_output_items
                        .push(ModelOutputItem::Message { text: text.clone() });
                }
                if !text.trim().is_empty() || reasoning_content.is_some() {
                    self.made_progress = true;
                    if let Some(reasoning) = &reasoning_content {
                        self.last_reasoning_content = Some(reasoning.clone());
                    }
                    if !text.trim().is_empty() {
                        self.record_assistant_message(text.clone()).await?;
                    }
                    super::history::record_history_item(
                        self.store,
                        self.agent,
                        self.agent_id,
                        self.session_id,
                        if reasoning_content.is_some() {
                            ModelInputItem::AssistantTurn {
                                content: (!text.trim().is_empty()).then_some(text),
                                reasoning_content,
                                tool_calls: Vec::new(),
                            }
                        } else {
                            ModelInputItem::assistant_text(text)
                        },
                    )
                    .await?;
                }
            }
            ModelOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                raw_arguments,
            } => {
                self.made_progress = true;
                let call_id = normalized_call_id(call_id);
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
                    ModelInputItem::FunctionCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments: raw_arguments,
                    },
                )
                .await?;
                self.tool_calls.push((call_id, name, arguments));
            }
            ModelOutputItem::AssistantTurn {
                content,
                reasoning_content,
                tool_calls,
            } => {
                let mut output_tool_calls = Vec::new();
                let assistant_tool_calls = tool_calls
                    .into_iter()
                    .map(|tool_call| {
                        let call_id = normalized_call_id(tool_call.call_id);
                        let name = tool_call.name;
                        let arguments = tool_call.arguments;
                        let raw_arguments = tool_call.raw_arguments;
                        self.tool_calls
                            .push((call_id.clone(), name.clone(), arguments.clone()));
                        output_tool_calls.push(mai_protocol::ModelOutputToolCall {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            arguments,
                            raw_arguments: raw_arguments.clone(),
                        });
                        ModelToolCall {
                            call_id,
                            name,
                            arguments: raw_arguments,
                        }
                    })
                    .collect::<Vec<_>>();
                let content = content
                    .or_else(|| (!active.text.trim().is_empty()).then_some(active.text))
                    .filter(|text| !text.trim().is_empty());
                let reasoning_content = reasoning_content
                    .or_else(|| (!active.reasoning.trim().is_empty()).then_some(active.reasoning))
                    .filter(|reasoning| !reasoning.trim().is_empty());
                let has_content = content.is_some();
                let has_reasoning = reasoning_content.is_some();
                self.final_output_items
                    .push(ModelOutputItem::AssistantTurn {
                        content: content.clone(),
                        reasoning_content: reasoning_content.clone(),
                        tool_calls: output_tool_calls,
                    });
                if has_content || has_reasoning || !assistant_tool_calls.is_empty() {
                    self.made_progress = true;
                    if let Some(reasoning) = &reasoning_content {
                        self.last_reasoning_content = Some(reasoning.clone());
                    }
                    super::history::record_history_item(
                        self.store,
                        self.agent,
                        self.agent_id,
                        self.session_id,
                        ModelInputItem::AssistantTurn {
                            content: content.clone(),
                            reasoning_content: reasoning_content.clone(),
                            tool_calls: assistant_tool_calls,
                        },
                    )
                    .await?;
                }
                if let Some(text) = content {
                    self.record_assistant_message(text).await?;
                }
            }
            ModelOutputItem::Other { raw } => {
                self.final_output_items.push(ModelOutputItem::Other { raw });
            }
        }
        Ok(())
    }

    async fn finish(mut self) -> Result<ModelTurnResult> {
        if !self.completed {
            return Err(model_error_to_runtime(mai_model::ModelError::Stream(
                "stream closed before response.completed".to_string(),
            )));
        }
        self.persist_legacy_delta_fallback().await?;
        if let Some(reasoning) = self.completed_reasoning_content() {
            self.events
                .publish(ServiceEventKind::ReasoningCompleted {
                    agent_id: self.agent_id,
                    session_id: Some(self.session_id),
                    turn_id: self.turn_id,
                    message_id: self.reasoning_id,
                    content: reasoning,
                })
                .await;
        }
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
        if !self.fallback_text.trim().is_empty() {
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
                ModelInputItem::assistant_text(text),
            )
            .await?;
        } else if !self.fallback_reasoning.trim().is_empty() {
            self.made_progress = true;
            let reasoning = self.fallback_reasoning.clone();
            self.last_reasoning_content = Some(reasoning.clone());
            self.final_output_items
                .push(ModelOutputItem::AssistantTurn {
                    content: None,
                    reasoning_content: Some(reasoning.clone()),
                    tool_calls: Vec::new(),
                });
            super::history::record_history_item(
                self.store,
                self.agent,
                self.agent_id,
                self.session_id,
                ModelInputItem::AssistantTurn {
                    content: None,
                    reasoning_content: Some(reasoning),
                    tool_calls: Vec::new(),
                },
            )
            .await?;
        }
        Ok(())
    }

    fn completed_reasoning_content(&self) -> Option<String> {
        self.last_reasoning_content
            .clone()
            .or_else(|| {
                (!self.fallback_reasoning.trim().is_empty())
                    .then(|| self.fallback_reasoning.clone())
            })
            .filter(|reasoning| !reasoning.trim().is_empty())
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
    consume_turn_stream(model, ctx, turn_model_state, stream, cancellation_token).await
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
    ctx: &TurnStreamContext<'_>,
    turn_model_state: &mut ModelTurnState,
    mut stream: ModelEventStream,
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
        let event = event.map_err(model_error_to_runtime)?;
        reducer.handle_event(event).await?;
    }
    let result = reducer.finish().await?;
    model.apply_completed_state(turn_model_state, result.response.id.as_deref());
    Ok(result)
}

pub(crate) fn model_error_to_runtime(err: mai_model::ModelError) -> RuntimeError {
    if matches!(err, mai_model::ModelError::Cancelled) {
        RuntimeError::TurnCancelled
    } else {
        RuntimeError::Model(err)
    }
}

fn normalized_call_id(call_id: String) -> String {
    if call_id.is_empty() {
        format!("call_{}", Uuid::new_v4())
    } else {
        call_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use mai_protocol::{
        AgentSessionSummary, AgentStatus, AgentSummary, ModelContentItem, ModelOutputToolCall, now,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;
    use tokio::sync::{Mutex, RwLock};

    use crate::state::{AgentSessionRecord, TurnControl};

    struct Harness {
        _dir: TempDir,
        store: Arc<ConfigStore>,
        events: RuntimeEvents,
        agent: Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
    }

    impl Harness {
        async fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = Arc::new(
                ConfigStore::open_with_config_path(
                    dir.path().join("runtime.sqlite3"),
                    dir.path().join("config.toml"),
                )
                .await
                .expect("store"),
            );
            let agent_id = Uuid::new_v4();
            let session_id = Uuid::new_v4();
            let turn_id = Uuid::new_v4();
            let created_at = now();
            let summary = AgentSummary {
                id: agent_id,
                parent_id: None,
                task_id: None,
                project_id: None,
                role: None,
                name: "stream-test-agent".to_string(),
                status: AgentStatus::RunningTurn,
                container_id: None,
                docker_image: "unused".to_string(),
                provider_id: "mock".to_string(),
                provider_name: "Mock".to_string(),
                model: "mock-model".to_string(),
                reasoning_effort: None,
                created_at,
                updated_at: created_at,
                current_turn: Some(turn_id),
                last_error: None,
                token_usage: TokenUsage::default(),
            };
            let session_summary = AgentSessionSummary {
                id: session_id,
                title: "Default".to_string(),
                created_at,
                updated_at: created_at,
                message_count: 0,
            };
            let agent = Arc::new(AgentRecord {
                summary: RwLock::new(summary.clone()),
                sessions: Mutex::new(vec![AgentSessionRecord {
                    summary: session_summary.clone(),
                    messages: Vec::new(),
                    history: Vec::new(),
                    last_context_tokens: None,
                    last_turn_response: None,
                }]),
                container: RwLock::new(None),
                mcp: RwLock::new(None),
                system_prompt: None,
                turn_lock: Mutex::new(()),
                cancel_requested: AtomicBool::new(false),
                active_turn: StdMutex::new(Some(TurnControl {
                    turn_id,
                    session_id,
                    cancellation_token: CancellationToken::new(),
                    abort_handle: None,
                })),
                pending_inputs: Mutex::new(VecDeque::new()),
            });
            store.save_agent(&summary, None).await.expect("save agent");
            store
                .save_agent_session(agent_id, &session_summary)
                .await
                .expect("save session");
            let events = RuntimeEvents::new(Arc::clone(&store), 0, Vec::new());
            Self {
                _dir: dir,
                store,
                events,
                agent,
                agent_id,
                session_id,
                turn_id,
            }
        }

        async fn consume(&self, events: Vec<ModelStreamEvent>) -> Result<ModelTurnResult> {
            let mut state = ModelTurnState::default();
            consume_turn_stream(
                &ModelClient::new(),
                &TurnStreamContext {
                    store: self.store.as_ref(),
                    events: &self.events,
                    agent: &self.agent,
                    agent_id: self.agent_id,
                    session_id: self.session_id,
                    turn_id: self.turn_id,
                },
                &mut state,
                event_stream(events),
                &CancellationToken::new(),
            )
            .await
        }

        async fn history(&self) -> Vec<ModelInputItem> {
            self.agent.sessions.lock().await[0].history.clone()
        }

        async fn messages(&self) -> Vec<mai_protocol::AgentMessage> {
            self.agent.sessions.lock().await[0].messages.clone()
        }
    }

    fn event_stream(events: Vec<ModelStreamEvent>) -> ModelEventStream {
        Box::pin(stream::iter(
            events.into_iter().map(Ok::<_, mai_model::ModelError>),
        ))
    }

    fn completed() -> ModelStreamEvent {
        ModelStreamEvent::Completed {
            id: Some("resp_1".to_string()),
            usage: Some(TokenUsage {
                input_tokens: 3,
                output_tokens: 2,
                total_tokens: 5,
            }),
            end_turn: Some(true),
        }
    }

    #[tokio::test]
    async fn delta_then_message_done_writes_one_final_assistant_history() {
        let harness = Harness::new().await;
        let result = harness
            .consume(vec![
                ModelStreamEvent::ResponseStarted {
                    id: Some("resp_1".to_string()),
                },
                ModelStreamEvent::OutputItemAdded {
                    output_index: 0,
                    item: ModelOutputItem::Message {
                        text: String::new(),
                    },
                },
                ModelStreamEvent::TextDelta {
                    output_index: 0,
                    content_index: Some(0),
                    delta: "hel".to_string(),
                },
                ModelStreamEvent::TextDelta {
                    output_index: 0,
                    content_index: Some(0),
                    delta: "lo".to_string(),
                },
                ModelStreamEvent::OutputItemDone {
                    output_index: 0,
                    item: ModelOutputItem::Message {
                        text: "hello".to_string(),
                    },
                },
                completed(),
            ])
            .await
            .expect("consume");

        assert!(result.made_progress);
        assert_eq!(result.last_assistant_text.as_deref(), Some("hello"));
        assert_eq!(result.response.output.len(), 1);
        assert!(matches!(
            &result.response.output[0],
            ModelOutputItem::Message { text } if text == "hello"
        ));
        let history = harness.history().await;
        assert_eq!(history.len(), 1);
        assert!(matches!(
            &history[0],
            ModelInputItem::Message { role, content }
                if role == "assistant"
                    && matches!(&content[0], ModelContentItem::OutputText { text } if text == "hello")
        ));
        let messages = harness.messages().await;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "hello");
        let event_snapshot = harness.events.snapshot().await;
        assert_eq!(
            event_snapshot
                .iter()
                .filter(|event| matches!(event.kind, ServiceEventKind::AgentMessageDelta { .. }))
                .count(),
            2
        );
        assert_eq!(
            event_snapshot
                .iter()
                .filter(|event| matches!(
                    event.kind,
                    ServiceEventKind::AgentMessageCompleted { .. }
                ))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn reasoning_delta_and_message_done_write_assistant_turn() {
        let harness = Harness::new().await;
        let result = harness
            .consume(vec![
                ModelStreamEvent::OutputItemAdded {
                    output_index: 0,
                    item: ModelOutputItem::Message {
                        text: String::new(),
                    },
                },
                ModelStreamEvent::ReasoningDelta {
                    output_index: 0,
                    content_index: Some(0),
                    delta: "thinking".to_string(),
                },
                ModelStreamEvent::TextDelta {
                    output_index: 0,
                    content_index: Some(1),
                    delta: "answer".to_string(),
                },
                ModelStreamEvent::OutputItemDone {
                    output_index: 0,
                    item: ModelOutputItem::Message {
                        text: "answer".to_string(),
                    },
                },
                completed(),
            ])
            .await
            .expect("consume");

        assert!(matches!(
            &result.response.output[0],
            ModelOutputItem::AssistantTurn {
                content: Some(content),
                reasoning_content: Some(reasoning),
                tool_calls,
            } if content == "answer" && reasoning == "thinking" && tool_calls.is_empty()
        ));
        let history = harness.history().await;
        assert!(matches!(
            &history[0],
            ModelInputItem::AssistantTurn {
                content: Some(content),
                reasoning_content: Some(reasoning),
                tool_calls,
            } if content == "answer" && reasoning == "thinking" && tool_calls.is_empty()
        ));
        let event_snapshot = harness.events.snapshot().await;
        assert!(event_snapshot.iter().any(|event| matches!(
            &event.kind,
            ServiceEventKind::ReasoningCompleted { content, .. } if content == "thinking"
        )));
    }

    #[tokio::test]
    async fn tool_preview_is_not_history_and_done_enqueues_final_arguments_once() {
        let harness = Harness::new().await;
        let result = harness
            .consume(vec![
                ModelStreamEvent::OutputItemAdded {
                    output_index: 0,
                    item: ModelOutputItem::FunctionCall {
                        call_id: "call_1".to_string(),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({}),
                        raw_arguments: String::new(),
                    },
                },
                ModelStreamEvent::ToolCallArgumentsDelta {
                    output_index: 0,
                    delta: "{\"path\":\"".to_string(),
                },
                ModelStreamEvent::ToolCallArgumentsDelta {
                    output_index: 0,
                    delta: "Cargo.toml\"}".to_string(),
                },
                ModelStreamEvent::OutputItemDone {
                    output_index: 0,
                    item: ModelOutputItem::FunctionCall {
                        call_id: "call_1".to_string(),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({ "path": "src/lib.rs" }),
                        raw_arguments: "{\"path\":\"src/lib.rs\"}".to_string(),
                    },
                },
                completed(),
            ])
            .await
            .expect("consume");

        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].0, "call_1");
        assert_eq!(result.tool_calls[0].1, "read_file");
        assert_eq!(result.tool_calls[0].2["path"], "src/lib.rs");
        let history = harness.history().await;
        assert_eq!(history.len(), 1);
        assert!(matches!(
            &history[0],
            ModelInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } if call_id == "call_1"
                && name == "read_file"
                && arguments == "{\"path\":\"src/lib.rs\"}"
        ));
        let event_snapshot = harness.events.snapshot().await;
        assert_eq!(
            event_snapshot
                .iter()
                .filter(|event| matches!(event.kind, ServiceEventKind::ToolCallDelta { .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn assistant_turn_done_enqueues_each_tool_call_once() {
        let harness = Harness::new().await;
        let result = harness
            .consume(vec![
                ModelStreamEvent::OutputItemDone {
                    output_index: 0,
                    item: ModelOutputItem::AssistantTurn {
                        content: Some("I'll inspect that.".to_string()),
                        reasoning_content: Some("Need file contents.".to_string()),
                        tool_calls: vec![ModelOutputToolCall {
                            call_id: "call_1".to_string(),
                            name: "read_file".to_string(),
                            arguments: serde_json::json!({ "path": "src/lib.rs" }),
                            raw_arguments: "{\"path\":\"src/lib.rs\"}".to_string(),
                        }],
                    },
                },
                completed(),
            ])
            .await
            .expect("consume");

        assert_eq!(result.tool_calls.len(), 1);
        let history = harness.history().await;
        assert_eq!(history.len(), 1);
        assert!(matches!(
            &history[0],
            ModelInputItem::AssistantTurn {
                content: Some(content),
                reasoning_content: Some(reasoning),
                tool_calls,
            } if content == "I'll inspect that."
                && reasoning == "Need file contents."
                && tool_calls.len() == 1
                && tool_calls[0].call_id == "call_1"
        ));
    }

    #[tokio::test]
    async fn stream_eof_before_completed_is_error() {
        let harness = Harness::new().await;
        let err = harness
            .consume(vec![ModelStreamEvent::TextDelta {
                output_index: 0,
                content_index: Some(0),
                delta: "partial".to_string(),
            }])
            .await
            .expect_err("stream should fail before completed");

        assert!(matches!(
            err,
            RuntimeError::Model(mai_model::ModelError::Stream(message))
                if message == "stream closed before response.completed"
        ));
        assert!(harness.history().await.is_empty());
    }

    #[tokio::test]
    async fn completed_delta_only_stream_uses_legacy_fallback_in_reducer() {
        let harness = Harness::new().await;
        let result = harness
            .consume(vec![
                ModelStreamEvent::TextDelta {
                    output_index: 0,
                    content_index: Some(0),
                    delta: "fallback answer".to_string(),
                },
                completed(),
            ])
            .await
            .expect("consume");

        assert_eq!(
            result.last_assistant_text.as_deref(),
            Some("fallback answer")
        );
        assert!(matches!(
            &result.response.output[0],
            ModelOutputItem::Message { text } if text == "fallback answer"
        ));
        let history = harness.history().await;
        assert_eq!(history.len(), 1);
        assert!(matches!(
            &history[0],
            ModelInputItem::Message { role, content }
                if role == "assistant"
                    && matches!(&content[0], ModelContentItem::OutputText { text } if text == "fallback answer")
        ));
    }
}
