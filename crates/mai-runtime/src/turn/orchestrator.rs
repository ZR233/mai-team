use std::future::Future;
use std::sync::Arc;

use mai_protocol::{
    AgentId, AgentStatus, MessageRole, ServiceEventKind, SessionId, SkillsConfigRequest, TurnId,
};
use mai_skills::{SkillInjections, SkillInput, SkillSelection, SkillsManager};
use pl_core::{HostMcpToolSpec, TurnErrorProjection, TurnReturnError};
use serde_json::json;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::deps::RuntimeDeps;
use crate::events::RuntimeEvents;
use crate::instructions::{self, ContainerSkillPaths};
use crate::state::{AgentRecord, RuntimeState};
use crate::turn::completion::TurnResult;
use crate::turn::persistence::AgentLogRecord;
use crate::{Result, RuntimeError};

/// 回合编排器依赖的运行时能力集合。
///
/// 该 trait 只描述一次用户回合所需的协调操作：加载 agent、准备容器、
/// 构建提示、压缩上下文、执行工具和推进排队输入。实现方必须保证返回的
/// future 可在线程间移动，并且不得在方法内部绕过传入的取消信号。
pub(crate) trait TurnOrchestratorOps: Send + Sync {
    fn runtime_handle(&self) -> Arc<crate::AgentRuntime>;

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

#[derive(Debug)]
struct TurnModelContext {
    provider_id: String,
    model_name: String,
    reasoning_effort: Option<String>,
    provider_selection: mai_store::ProviderSelection,
    visible_tool_names: pl_core::ToolVisibilitySet,
    tool_count: usize,
    product_tools: Vec<pl_model::ToolSchema>,
    mcp_tool_schemas: Vec<pl_model::ToolSchema>,
    instructions: String,
}

fn pl_turn_return_error(error: RuntimeError) -> TurnReturnError {
    let message = error.to_string();
    match error {
        RuntimeError::TurnCancelled => TurnReturnError::Cancelled,
        RuntimeError::AgentNotFound(_)
        | RuntimeError::TaskNotFound(_)
        | RuntimeError::ProjectNotFound(_)
        | RuntimeError::ProjectReviewRunNotFound(_)
        | RuntimeError::AgentBusy(_)
        | RuntimeError::TaskBusy(_)
        | RuntimeError::MissingContainer(_)
        | RuntimeError::SessionNotFound { .. }
        | RuntimeError::ToolTraceNotFound { .. }
        | RuntimeError::TurnNotFound { .. }
        | RuntimeError::Docker(_)
        | RuntimeError::Model(_)
        | RuntimeError::Mcp(_)
        | RuntimeError::Store(_)
        | RuntimeError::Skill(_)
        | RuntimeError::InvalidInput(_)
        | RuntimeError::Io(_)
        | RuntimeError::Http(_)
        | RuntimeError::Jwt(_) => TurnReturnError::Failed(message),
    }
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
        let projection = TurnErrorProjection::from_return_error(pl_turn_return_error(err));
        let (turn_status, agent_status) =
            super::core_adapter::mai_status_from_pl_outcome(projection.status);
        let completed = super::completion::complete_turn_if_current(
            deps.store.as_ref(),
            events,
            &agent,
            agent_id,
            TurnResult {
                turn_id,
                status: turn_status,
                agent_status,
                final_text: None,
                error: projection.error_message.clone(),
            },
        )
        .await
        .unwrap_or(false);
        if completed {
            ops.start_next_queued_input_after_turn(agent_id).await;
            if projection.should_publish_error
                && let Some(message) = projection.error_message
            {
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
    super::history::record_message(
        deps.store.as_ref(),
        &agent,
        agent_id,
        session_id,
        MessageRole::User,
        message.clone(),
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
        super::tool_visibility::visible_tool_names(state, &agent, &mcp_tools)
            .await
            .to_btree_set()
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
        let visible_tools =
            super::tool_visibility::visible_tool_names(state, &agent, &mcp_tools).await;
        let product_tools = visible_tools.filter_schemas(mai_tools::build_tool_schemas());
        let mcp_tool_schemas = pl_core::host_mcp_tool_schemas(
            mcp_tools
                .iter()
                .filter(|tool| visible_tools.contains(&tool.model_name))
                .map(host_mcp_tool_spec),
        );
        let tool_count = visible_tools.len();
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
                    "tool_count": tool_count,
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
            visible_tool_names: visible_tools,
            tool_count,
            product_tools,
            mcp_tool_schemas,
            instructions,
        }
    };
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
        super::history::session_history(deps.store.as_ref(), &agent, agent_id, session_id).await?;
    let history_duration_ms = u128_to_u64(history_started.elapsed().as_millis());
    let message = message_with_skill_fragment(
        message,
        instructions::skill_user_fragment(&skill_injections, &container_skill_paths),
    );
    super::persistence::record_agent_log(
        deps.store.as_ref(),
        AgentLogRecord {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            level: "info",
            category: "runtime",
            message: "delegating turn to pl-core",
            details: json!({
                "provider_id": model_context.provider_id,
                "model": model_context.model_name,
                "tool_count": model_context.tool_count,
                "history_items": history.len(),
                "history_load_ms": history_duration_ms,
            }),
        },
    )
    .await;
    super::core_adapter::run_pure_core_turn(super::core_adapter::PureCoreTurnContext {
        runtime: ops.runtime_handle(),
        agent: Arc::clone(&agent),
        agent_id,
        session_id,
        turn_id,
        message,
        provider_selection: model_context.provider_selection,
        reasoning_effort: model_context.reasoning_effort,
        instructions: model_context.instructions,
        visible_tool_names: model_context.visible_tool_names,
        product_tools: model_context.product_tools,
        mcp_tool_schemas: model_context.mcp_tool_schemas,
        history,
        cancellation_token,
    })
    .await
}

fn host_mcp_tool_spec(tool: &mai_mcp::McpTool) -> HostMcpToolSpec {
    HostMcpToolSpec {
        model_name: tool.model_name.clone(),
        server: tool.server.clone(),
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema: tool.input_schema.clone(),
    }
}

fn message_with_skill_fragment(message: String, fragment: Option<pl_protocol::Message>) -> String {
    pl_core::append_message_fragment_text(message, fragment.as_ref())
}

fn u128_to_u64(value: u128) -> u64 {
    value.min(u64::MAX as u128) as u64
}
