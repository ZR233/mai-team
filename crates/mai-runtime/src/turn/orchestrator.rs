use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use mai_protocol::{
    AgentId, AgentStatus, MessageRole, ServiceEventKind, SessionId, SkillsConfigRequest, TurnId,
    TurnStatus,
};
use mai_skills::{SkillInjections, SkillInput, SkillSelection, SkillsManager};
use mai_tools::build_tool_definitions_with_filter;
use pl_core::CoreSession;
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::deps::RuntimeDeps;
use crate::events::RuntimeEvents;
use crate::instructions::{self, ContainerSkillPaths};
use crate::state::{AgentRecord, RuntimeState};
use crate::turn::completion::TurnResult;
use crate::turn::context::{ContextCompactionOutcome, ContextCompactionRequest};
use crate::turn::model_stream::TurnModelContext;
use crate::turn::persistence::AgentLogRecord;
use crate::turn::tools::ToolExecution;
use crate::{Result, RuntimeError};

/// 回合编排器依赖的运行时能力集合。
///
/// 该 trait 只描述一次用户回合所需的协调操作：加载 agent、准备容器、
/// 构建提示、压缩上下文、执行工具和推进排队输入。实现方必须保证返回的
/// future 可在线程间移动，并且不得在方法内部绕过传入的取消信号。
pub(crate) trait TurnOrchestratorOps: Send + Sync {
    fn agent(&self, agent_id: AgentId) -> impl Future<Output = Result<Arc<AgentRecord>>> + Send;

    fn ensure_agent_container_for_turn(
        &self,
        agent: &Arc<AgentRecord>,
        status: AgentStatus,
        turn_id: TurnId,
        cancellation_token: &CancellationToken,
    ) -> impl Future<Output = Result<()>> + Send;

    fn refresh_project_skills_for_agent(
        &self,
        agent: &AgentRecord,
    ) -> impl Future<Output = Result<()>> + Send;

    fn skills_manager_for_agent(
        &self,
        agent: &AgentRecord,
    ) -> impl Future<Output = Result<SkillsManager>> + Send;

    fn sync_agent_skills_to_container(
        &self,
        agent: &Arc<AgentRecord>,
        skills_manager: &SkillsManager,
        skills_config: &SkillsConfigRequest,
    ) -> impl Future<Output = Result<ContainerSkillPaths>> + Send;

    fn maybe_auto_compact(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        request: ContextCompactionRequest,
        cancellation_token: &CancellationToken,
    ) -> impl Future<Output = Result<ContextCompactionOutcome>> + Send;

    fn agent_mcp_tools(
        &self,
        agent: &AgentRecord,
    ) -> impl Future<Output = Vec<mai_mcp::McpTool>> + Send;

    fn project_skill_read_guard(
        &self,
        agent: &AgentRecord,
    ) -> impl Future<Output = Option<tokio::sync::OwnedRwLockReadGuard<()>>> + Send;

    fn inject_project_mcp_tools(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
        cancellation_token: &CancellationToken,
    ) -> impl Future<Output = Result<()>> + Send;

    fn build_instructions(
        &self,
        agent: &AgentRecord,
        skills_manager: &SkillsManager,
        skill_injections: &SkillInjections,
        skills_config: &SkillsConfigRequest,
        mcp_tools: &[mai_mcp::McpTool],
        container_skill_paths: &ContainerSkillPaths,
    ) -> impl Future<Output = Result<String>> + Send;

    fn set_turn_status(
        &self,
        agent: &Arc<AgentRecord>,
        turn_id: TurnId,
        cancellation_token: &CancellationToken,
        enforce_current_turn: bool,
        status: AgentStatus,
    ) -> impl Future<Output = Result<()>> + Send;

    fn execute_tool(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        turn_id: TurnId,
        name: &str,
        arguments: Value,
        cancellation_token: CancellationToken,
    ) -> impl Future<Output = Result<ToolExecution>> + Send;

    fn start_next_queued_input_after_turn(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = ()> + Send;
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
    ops: &(impl TurnOrchestratorOps + ?Sized),
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
    ops: &(impl TurnOrchestratorOps + ?Sized),
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
        .maybe_auto_compact(
            &agent,
            agent_id,
            session_id,
            turn_id,
            ContextCompactionRequest::last_context_only(),
            &cancellation_token,
        )
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
        return Err(err);
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
        super::history::user_text_message(message.clone()),
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
        let model_selection = crate::model_selection::resolve_agent_model_selection(
            deps,
            events,
            &agent,
            agent_id,
            Some(session_id),
            Some(turn_id),
        )
        .await?;
        let provider_id = model_selection.provider_id;
        let model_name = model_selection.model_name;
        let reasoning_effort = model_selection.reasoning_effort;
        let provider_selection = model_selection.provider_selection;
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
    let mut core_session = CoreSession::new();
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
        let history_started = Instant::now();
        let history =
            super::history::session_history(deps.store.as_ref(), &agent, agent_id, session_id)
                .await?;
        let mut session_messages = history;
        if core_session.previous_response_id().is_none()
            && let Some(skill_fragment) =
                instructions::skill_user_fragment(&skill_injections, &container_skill_paths)
        {
            session_messages.push(skill_fragment);
        }
        core_session.replace_messages_preserving_continuation(session_messages);
        let estimated_request_tokens = super::context::estimate_model_request_tokens(
            &model_context.instructions,
            core_session.messages(),
            &model_context.tools,
        );
        match ops
            .maybe_auto_compact(
                &agent,
                agent_id,
                session_id,
                turn_id,
                ContextCompactionRequest::from_estimate(estimated_request_tokens),
                &cancellation_token,
            )
            .await
        {
            Ok(ContextCompactionOutcome::Compacted { .. }) => {
                core_session.reset_continuation();
                let mut session_messages = super::history::session_history(
                    deps.store.as_ref(),
                    &agent,
                    agent_id,
                    session_id,
                )
                .await?;
                if let Some(skill_fragment) =
                    instructions::skill_user_fragment(&skill_injections, &container_skill_paths)
                {
                    session_messages.push(skill_fragment);
                }
                core_session.replace_messages_preserving_continuation(session_messages);
            }
            Ok(ContextCompactionOutcome::Skipped) => {}
            Err(err) => {
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
                return Err(err);
            }
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
            &mut core_session,
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
                    "history_items": core_session.len(),
                    "history_load_ms": history_duration_ms,
                    "duration_ms": model_duration_ms,
                    "usage": model_turn.response.usage,
                }),
            },
        )
        .await;

        let last_model_total_tokens = model_turn
            .response
            .usage
            .as_ref()
            .map(|usage| usage.total_tokens);
        if let Some(usage) = model_turn.response.usage.clone() {
            super::accounting::record_model_usage(
                deps.store.as_ref(),
                events,
                &agent,
                agent_id,
                session_id,
                &usage,
            )
            .await?;
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
                super::history::user_text_message(diagnostic),
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

        if let Some(tokens) = last_model_total_tokens {
            super::history::record_session_context_tokens(
                deps.store.as_ref(),
                &agent,
                agent_id,
                session_id,
                tokens,
            )
            .await?;
        }

        let history =
            super::history::session_history(deps.store.as_ref(), &agent, agent_id, session_id)
                .await?;
        let estimated_request_tokens = super::context::estimate_model_request_tokens(
            &model_context.instructions,
            &history,
            &model_context.tools,
        );
        let compaction_request = if let Some(tokens) = last_model_total_tokens {
            ContextCompactionRequest::after_model_response(estimated_request_tokens, tokens)
        } else {
            ContextCompactionRequest::from_estimate(estimated_request_tokens)
        };
        match ops
            .maybe_auto_compact(
                &agent,
                agent_id,
                session_id,
                turn_id,
                compaction_request,
                &cancellation_token,
            )
            .await
        {
            Ok(ContextCompactionOutcome::Compacted { .. }) => {
                core_session.reset_continuation();
            }
            Ok(ContextCompactionOutcome::Skipped) => {}
            Err(err) => {
                if matches!(err, RuntimeError::TurnCancelled) {
                    return Err(err);
                }
                tracing::warn!("auto context compaction failed after tool execution: {err}");
                super::persistence::record_agent_log(
                    deps.store.as_ref(),
                    AgentLogRecord {
                        agent_id,
                        session_id: Some(session_id),
                        turn_id: Some(turn_id),
                        level: "warn",
                        category: "context",
                        message: "auto context compaction failed",
                        details: json!({ "stage": "after_tool_execution", "error": err.to_string() }),
                    },
                )
                .await;
                return Err(err);
            }
        }
    }
}

fn u128_to_u64(value: u128) -> u64 {
    value.min(u64::MAX as u128) as u64
}
