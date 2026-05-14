use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use mai_model::ModelTurnState;
use mai_protocol::{
    AgentId, AgentStatus, MessageRole, ModelInputItem, ServiceEventKind, SessionId,
    SkillsConfigRequest, TurnId, TurnStatus, now,
};
use mai_skills::{SkillInjections, SkillInput, SkillSelection, SkillsManager};
use mai_tools::build_tool_definitions_with_filter;
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::deps::RuntimeDeps;
use crate::events::RuntimeEvents;
use crate::instructions::{self, ContainerSkillPaths};
use crate::state::{AgentRecord, RuntimeState};
use crate::turn::completion::TurnResult;
use crate::turn::model_stream::TurnModelContext;
use crate::turn::persistence::AgentLogRecord;
use crate::turn::tools::ToolExecution;
use crate::{Result, RuntimeError};

#[async_trait]
pub(crate) trait TurnOrchestratorOps: Send + Sync {
    async fn agent(&self, agent_id: AgentId) -> Result<Arc<AgentRecord>>;

    async fn ensure_agent_container_for_turn(
        &self,
        agent: &Arc<AgentRecord>,
        status: AgentStatus,
        turn_id: TurnId,
        cancellation_token: &CancellationToken,
    ) -> Result<()>;

    async fn refresh_project_skills_for_agent(&self, agent: &AgentRecord) -> Result<()>;

    async fn skills_manager_for_agent(&self, agent: &AgentRecord) -> Result<SkillsManager>;

    async fn sync_agent_skills_to_container(
        &self,
        agent: &Arc<AgentRecord>,
        skills_manager: &SkillsManager,
        skills_config: &SkillsConfigRequest,
    ) -> Result<ContainerSkillPaths>;

    async fn maybe_auto_compact(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        cancellation_token: &CancellationToken,
    ) -> Result<()>;

    async fn agent_mcp_tools(&self, agent: &AgentRecord) -> Vec<mai_mcp::McpTool>;

    async fn project_skill_read_guard(
        &self,
        agent: &AgentRecord,
    ) -> Option<tokio::sync::OwnedRwLockReadGuard<()>>;

    async fn inject_project_mcp_tools(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
        cancellation_token: &CancellationToken,
    ) -> Result<()>;

    async fn build_instructions(
        &self,
        agent: &AgentRecord,
        skills_manager: &SkillsManager,
        skill_injections: &SkillInjections,
        skills_config: &SkillsConfigRequest,
        mcp_tools: &[mai_mcp::McpTool],
        container_skill_paths: &ContainerSkillPaths,
    ) -> Result<String>;

    async fn set_turn_status(
        &self,
        agent: &Arc<AgentRecord>,
        turn_id: TurnId,
        cancellation_token: &CancellationToken,
        enforce_current_turn: bool,
        status: AgentStatus,
    ) -> Result<()>;

    async fn execute_tool(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        turn_id: TurnId,
        name: &str,
        arguments: Value,
        cancellation_token: CancellationToken,
    ) -> Result<ToolExecution>;

    async fn persist_agent(&self, agent: &AgentRecord) -> Result<()>;

    async fn start_next_queued_input_after_turn(&self, agent_id: AgentId);
}

pub(crate) struct TurnRequest {
    pub(crate) agent_id: AgentId,
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) message: String,
    pub(crate) skill_mentions: Vec<String>,
    pub(crate) cancellation_token: CancellationToken,
}

pub(crate) async fn run_turn(
    deps: &RuntimeDeps,
    state: &RuntimeState,
    events: &RuntimeEvents,
    ops: &dyn TurnOrchestratorOps,
    request: TurnRequest,
) {
    let agent_id = request.agent_id;
    let session_id = request.session_id;
    let turn_id = request.turn_id;
    let result = run_turn_inner(deps, state, events, ops, request).await;
    if let Err(err) = result
        && let Ok(agent) = ops.agent(agent_id).await
    {
        if matches!(err, RuntimeError::TurnCancelled) {
            if let Ok(completed) = super::completion::complete_turn_if_current(
                deps.store.as_ref(),
                events,
                &agent,
                agent_id,
                TurnResult {
                    turn_id,
                    status: TurnStatus::Cancelled,
                    agent_status: AgentStatus::Cancelled,
                    final_text: None,
                    error: None,
                },
            )
            .await
                && completed
            {
                ops.start_next_queued_input_after_turn(agent_id).await;
            }
            return;
        }
        let message = err.to_string();
        let completed = super::completion::complete_turn_if_current(
            deps.store.as_ref(),
            events,
            &agent,
            agent_id,
            TurnResult {
                turn_id,
                status: TurnStatus::Failed,
                agent_status: AgentStatus::Failed,
                final_text: None,
                error: Some(message.clone()),
            },
        )
        .await
        .unwrap_or(false);
        if completed {
            ops.start_next_queued_input_after_turn(agent_id).await;
            events
                .publish(ServiceEventKind::Error {
                    agent_id: Some(agent_id),
                    session_id: Some(session_id),
                    turn_id: Some(turn_id),
                    message,
                })
                .await;
        }
    }
}

pub(crate) async fn run_turn_inner(
    deps: &RuntimeDeps,
    state: &RuntimeState,
    events: &RuntimeEvents,
    ops: &dyn TurnOrchestratorOps,
    request: TurnRequest,
) -> Result<()> {
    let TurnRequest {
        agent_id,
        session_id,
        turn_id,
        message,
        skill_mentions,
        cancellation_token,
    } = request;
    let agent = ops.agent(agent_id).await?;
    let _turn_guard = agent.turn_lock.lock().await;
    let enforce_current_turn = agent.summary.read().await.current_turn == Some(turn_id);
    if cancellation_token.is_cancelled() {
        return Err(RuntimeError::TurnCancelled);
    }
    ops.ensure_agent_container_for_turn(
        &agent,
        AgentStatus::RunningTurn,
        turn_id,
        &cancellation_token,
    )
    .await?;
    if cancellation_token.is_cancelled() {
        return Err(RuntimeError::TurnCancelled);
    }
    events
        .publish(ServiceEventKind::TurnStarted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
        })
        .await;
    super::persistence::record_agent_log(
        deps.store.as_ref(),
        AgentLogRecord {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            level: "info",
            category: "turn",
            message: "turn started",
            details: json!({}),
        },
    )
    .await;

    if let Err(err) = ops.refresh_project_skills_for_agent(&agent).await {
        if matches!(err, RuntimeError::TurnCancelled) {
            return Err(err);
        }
        tracing::warn!(agent_id = %agent_id, "failed to refresh project skills before turn: {err}");
        super::persistence::record_agent_log(
            deps.store.as_ref(),
            AgentLogRecord {
                agent_id,
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                level: "warn",
                category: "skills",
                message: "project skill refresh failed",
                details: json!({ "error": err.to_string() }),
            },
        )
        .await;
    }
    let skills_config = deps.store.load_skills_config().await?;
    let skills_manager = ops.skills_manager_for_agent(&agent).await?;
    let container_skill_paths = ops
        .sync_agent_skills_to_container(&agent, &skills_manager, &skills_config)
        .await?;
    if let Err(err) = ops
        .maybe_auto_compact(&agent, agent_id, session_id, turn_id, &cancellation_token)
        .await
    {
        if matches!(err, RuntimeError::TurnCancelled) {
            return Err(err);
        }
        tracing::warn!("auto context compaction failed before user message: {err}");
        super::persistence::record_agent_log(
            deps.store.as_ref(),
            AgentLogRecord {
                agent_id,
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                level: "warn",
                category: "context",
                message: "auto context compaction failed",
                details: json!({ "stage": "before_user_message", "error": err.to_string() }),
            },
        )
        .await;
    }
    super::history::record_message(
        deps.store.as_ref(),
        &agent,
        agent_id,
        session_id,
        MessageRole::User,
        message.clone(),
    )
    .await?;
    super::history::record_history_item(
        deps.store.as_ref(),
        &agent,
        agent_id,
        session_id,
        ModelInputItem::user_text(message.clone()),
    )
    .await?;
    events
        .publish(ServiceEventKind::AgentMessage {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            role: MessageRole::User,
            content: message.clone(),
        })
        .await;

    let reserved_tool_names = {
        let mcp_tools = ops.agent_mcp_tools(&agent).await;
        super::tools::visible_tool_names(state, &agent, &mcp_tools)
            .await
            .into_iter()
            .collect::<BTreeSet<_>>()
    };
    let skill_injections = {
        let _project_skill_guard = ops.project_skill_read_guard(&agent).await;
        skills_manager.build_injections_for_input(
            SkillInput {
                text: Some(&message),
                selections: skill_mentions
                    .iter()
                    .map(|mention| SkillSelection::from_mention(mention.clone()))
                    .collect(),
                reserved_names: reserved_tool_names,
            },
            &skills_config,
        )?
    };
    if !skill_injections.items.is_empty() {
        events
            .publish(ServiceEventKind::SkillsActivated {
                agent_id,
                session_id: Some(session_id),
                turn_id,
                skills: instructions::skill_activation_info(
                    &skill_injections,
                    &container_skill_paths,
                ),
            })
            .await;
        super::persistence::record_agent_log(
            deps.store.as_ref(),
            AgentLogRecord {
                agent_id,
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                level: "info",
                category: "skills",
                message: "skills activated",
                details: json!({ "count": skill_injections.items.len() }),
            },
        )
        .await;
    }
    ops.inject_project_mcp_tools(&agent, agent_id, session_id, &cancellation_token)
        .await?;
    let model_context = {
        let context_started = Instant::now();
        let mcp_tools = ops.agent_mcp_tools(&agent).await;
        let visible_tools = super::tools::visible_tool_names(state, &agent, &mcp_tools).await;
        let tools =
            build_tool_definitions_with_filter(&mcp_tools, |name| visible_tools.contains(name));
        let instructions = {
            let _project_skill_guard = ops.project_skill_read_guard(&agent).await;
            ops.build_instructions(
                &agent,
                &skills_manager,
                &skill_injections,
                &skills_config,
                &mcp_tools,
                &container_skill_paths,
            )
            .await?
        };
        let summary = agent.summary.read().await.clone();
        let provider_id = summary.provider_id.clone();
        let model_name = summary.model.clone();
        let reasoning_effort = summary.reasoning_effort;
        let provider_selection = deps
            .store
            .resolve_provider(Some(&provider_id), Some(&model_name))
            .await?;
        super::persistence::record_agent_log(
            deps.store.as_ref(),
            AgentLogRecord {
                agent_id,
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                level: "info",
                category: "runtime",
                message: "turn model context prepared",
                details: json!({
                    "provider_id": provider_id,
                    "model": model_name,
                    "tool_count": tools.len(),
                    "mcp_tool_count": mcp_tools.len(),
                    "instructions_bytes": instructions.len(),
                    "duration_ms": u128_to_u64(context_started.elapsed().as_millis()),
                }),
            },
        )
        .await;
        TurnModelContext {
            provider_id,
            model_name,
            reasoning_effort,
            provider_selection,
            tools,
            instructions,
        }
    };
    let mut last_assistant_text: Option<String> = None;
    let mut turn_model_state = ModelTurnState::default();
    let mut empty_progress_count: usize = 0;
    loop {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        ops.set_turn_status(
            &agent,
            turn_id,
            &cancellation_token,
            enforce_current_turn,
            AgentStatus::RunningTurn,
        )
        .await?;
        if let Err(err) = ops
            .maybe_auto_compact(&agent, agent_id, session_id, turn_id, &cancellation_token)
            .await
        {
            if matches!(err, RuntimeError::TurnCancelled) {
                return Err(err);
            }
            tracing::warn!("auto context compaction failed before model request: {err}");
            super::persistence::record_agent_log(
                deps.store.as_ref(),
                AgentLogRecord {
                    agent_id,
                    session_id: Some(session_id),
                    turn_id: Some(turn_id),
                    level: "warn",
                    category: "context",
                    message: "auto context compaction failed",
                    details: json!({ "stage": "before_model_request", "error": err.to_string() }),
                },
            )
            .await;
        }
        let history_started = Instant::now();
        let mut history =
            super::history::session_history(deps.store.as_ref(), &agent, agent_id, session_id)
                .await?;
        if turn_model_state.previous_response_id.is_none()
            && let Some(skill_fragment) =
                instructions::skill_user_fragment(&skill_injections, &container_skill_paths)
        {
            history.push(skill_fragment);
        }
        let history_duration_ms = u128_to_u64(history_started.elapsed().as_millis());
        let model_started = Instant::now();
        let model_turn = super::model_stream::run_model_stream_turn(
            &deps.model,
            &super::model_stream::TurnStreamContext {
                store: deps.store.as_ref(),
                events,
                agent: &agent,
                agent_id,
                session_id,
                turn_id,
            },
            &model_context,
            &history,
            &mut turn_model_state,
            &cancellation_token,
        )
        .await?;
        let model_duration_ms = u128_to_u64(model_started.elapsed().as_millis());
        super::persistence::record_agent_log(
            deps.store.as_ref(),
            AgentLogRecord {
                agent_id,
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                level: "info",
                category: "model",
                message: "model stream completed",
                details: json!({
                    "provider_id": model_context.provider_id,
                    "model": model_context.model_name,
                    "output_items": model_turn.response.output.len(),
                    "history_items": history.len(),
                    "history_load_ms": history_duration_ms,
                    "duration_ms": model_duration_ms,
                    "usage": model_turn.response.usage,
                }),
            },
        )
        .await;

        if let Some(usage) = model_turn.response.usage.clone() {
            {
                let mut summary = agent.summary.write().await;
                summary.token_usage.add(&usage);
                summary.updated_at = now();
            }
            ops.persist_agent(&agent).await?;
            super::history::record_session_context_tokens(
                deps.store.as_ref(),
                &agent,
                agent_id,
                session_id,
                usage.total_tokens,
            )
            .await?;
        }

        let tool_calls = model_turn.tool_calls;
        let made_progress = model_turn.made_progress;
        if model_turn.last_assistant_text.is_some() {
            last_assistant_text = model_turn.last_assistant_text;
        }
        let acknowledged_history_len =
            super::history::raw_session_history_len(&agent, agent_id, session_id).await?;
        turn_model_state.acknowledge_history_len(acknowledged_history_len);

        if !made_progress {
            empty_progress_count = empty_progress_count.saturating_add(1);
            let diagnostic = format!(
                "Runtime diagnostic: the previous model response produced no assistant text and no tool calls (empty_progress_count={empty_progress_count}). Decide whether to continue, ask the user for clarification, retry with a different approach, or explain the issue."
            );
            super::history::record_history_item(
                deps.store.as_ref(),
                &agent,
                agent_id,
                session_id,
                ModelInputItem::user_text(diagnostic),
            )
            .await?;
            continue;
        }
        empty_progress_count = 0;

        if tool_calls.is_empty() {
            super::completion::finish_turn(
                deps.store.as_ref(),
                events,
                &agent,
                agent_id,
                session_id,
                TurnResult {
                    turn_id,
                    status: TurnStatus::Completed,
                    agent_status: AgentStatus::Completed,
                    final_text: last_assistant_text,
                    error: None,
                },
            )
            .await?;
            ops.start_next_queued_input_after_turn(agent_id).await;
            return Ok(());
        }

        ops.set_turn_status(
            &agent,
            turn_id,
            &cancellation_token,
            enforce_current_turn,
            AgentStatus::WaitingTool,
        )
        .await?;
        let mut should_end_turn = false;
        for (call_id, name, arguments) in tool_calls {
            if cancellation_token.is_cancelled() {
                return Err(RuntimeError::TurnCancelled);
            }
            let tool_agent = Arc::clone(&agent);
            let execution = super::tools::run_tool_call(
                &super::tools::ToolCallContext {
                    store: deps.store.as_ref(),
                    events,
                    agent: &agent,
                    agent_id,
                    session_id,
                    turn_id,
                },
                super::tools::ToolCallInfo {
                    call_id: &call_id,
                    name: &name,
                    arguments,
                },
                |arguments| {
                    let cancellation_token = cancellation_token.clone();
                    let name = name.clone();
                    async move {
                        ops.execute_tool(
                            &tool_agent,
                            agent_id,
                            turn_id,
                            &name,
                            arguments,
                            cancellation_token,
                        )
                        .await
                    }
                },
            )
            .await?;
            if execution.ends_turn {
                should_end_turn = true;
            }
            if cancellation_token.is_cancelled() {
                return Err(RuntimeError::TurnCancelled);
            }
        }

        if should_end_turn {
            super::completion::finish_turn(
                deps.store.as_ref(),
                events,
                &agent,
                agent_id,
                session_id,
                TurnResult {
                    turn_id,
                    status: TurnStatus::Completed,
                    agent_status: AgentStatus::Completed,
                    final_text: last_assistant_text,
                    error: None,
                },
            )
            .await?;
            ops.start_next_queued_input_after_turn(agent_id).await;
            return Ok(());
        }
    }
}

fn u128_to_u64(value: u128) -> u64 {
    value.min(u64::MAX as u128) as u64
}
