use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mai_mcp::McpTool;
#[cfg(test)]
use mai_protocol::ToolTraceDetail;
use mai_protocol::{
    AgentId, AgentRole, AgentSummary, ArtifactInfo, ServiceEventKind, SessionId, TaskReview,
    TaskSummary, TodoItem, ToolOutputArtifactInfo, TurnId, UserInputOption, UserInputQuestion, now,
    preview,
};
#[cfg(test)]
use mai_store::ConfigStore;
use mai_tools::{RoutedTool, route_tool};
use serde_json::{Value, json};
#[cfg(test)]
use tokio::time::Instant;
use uuid::Uuid;

use crate::agents;
use crate::events::RuntimeEvents;
use crate::state::{AgentRecord, CollabInput, RuntimeState};
use crate::turn::container::{ContainerToolContext, ContainerToolOps};
#[cfg(test)]
use crate::turn::persistence::AgentLogRecord;
use crate::{Result, RuntimeError};

const TOKEN_ESTIMATE_BYTES: usize = 4;
pub(crate) const DEFAULT_MODEL_TOOL_OUTPUT_TOKENS: usize = 10_000;
const DEFAULT_WAIT_AGENT_OBSERVATION_SECS: u64 = 30;
#[cfg(test)]
const DEFAULT_MODEL_TOOL_OUTPUT_BYTES: usize =
    DEFAULT_MODEL_TOOL_OUTPUT_TOKENS * TOKEN_ESTIMATE_BYTES;

#[derive(Debug)]
pub(crate) struct ToolExecution {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) success: bool,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) output: String,
    pub(crate) model_output: String,
    pub(crate) ends_turn: bool,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) output_artifacts: Vec<ToolOutputArtifactInfo>,
}

impl ToolExecution {
    pub(crate) fn new(success: bool, output: String, ends_turn: bool) -> Self {
        Self::with_model_tokens(
            success,
            output,
            ends_turn,
            DEFAULT_MODEL_TOOL_OUTPUT_TOKENS,
            Vec::new(),
        )
    }

    pub(crate) fn with_model_tokens(
        success: bool,
        output: String,
        ends_turn: bool,
        max_output_tokens: usize,
        output_artifacts: Vec<ToolOutputArtifactInfo>,
    ) -> Self {
        let model_output = bounded_model_tool_output_with_tokens(&output, max_output_tokens);
        Self::with_model_output(success, output, model_output, ends_turn, output_artifacts)
    }

    pub(crate) fn with_model_output(
        success: bool,
        output: String,
        model_output: String,
        ends_turn: bool,
        output_artifacts: Vec<ToolOutputArtifactInfo>,
    ) -> Self {
        Self {
            success,
            output,
            model_output,
            ends_turn,
            output_artifacts,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ToolOutputCapture {
    pub(crate) call_id: String,
    pub(crate) stdout_id: String,
    pub(crate) stderr_id: String,
    pub(crate) stdout_path: PathBuf,
    pub(crate) stderr_path: PathBuf,
    pub(crate) stdout_name: String,
    pub(crate) stderr_name: String,
}

pub(crate) struct ToolDispatchContext<'a, O: ToolDispatchOps + ContainerToolOps + ?Sized> {
    pub(crate) state: &'a RuntimeState,
    pub(crate) container: ContainerToolContext<'a, O>,
    pub(crate) events: &'a RuntimeEvents,
    pub(crate) ops: &'a O,
}

#[derive(Debug)]
pub(crate) struct SpawnAgentToolRequest {
    pub(crate) name: Option<String>,
    pub(crate) role: AgentRole,
    pub(crate) legacy_role: Option<AgentRole>,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) fork_context: bool,
    pub(crate) collab_input: CollabInput,
}

#[derive(Debug)]
pub(crate) struct SpawnAgentToolResult {
    pub(crate) agent: AgentSummary,
    pub(crate) turn_id: Option<TurnId>,
}

#[derive(Debug, Clone)]
pub(crate) struct QueueProjectReviewPr {
    pub(crate) number: u64,
    pub(crate) head_sha: Option<String>,
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct GithubApiRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) body: Option<Value>,
}

/// 非容器工具依赖的运行时能力集合。
///
/// 该 trait 是工具分发表与 `AgentRuntime` 之间的内部边界，覆盖协作 agent、
/// MCP、任务、项目 Git/GitHub 等副作用。实现方必须保持调用可取消，并让返回
/// future 满足 `Send`，以便回合执行可以在线程池中安全推进。
pub(crate) trait ToolDispatchOps: Send + Sync {
    fn spawn_agent_from_tool(
        &self,
        parent_agent_id: AgentId,
        request: SpawnAgentToolRequest,
    ) -> impl Future<Output = Result<SpawnAgentToolResult>> + Send;
    fn send_input_to_agent(
        &self,
        target: AgentId,
        session_id: Option<SessionId>,
        message: String,
        skill_mentions: Vec<String>,
        interrupt: bool,
    ) -> impl Future<Output = Result<Value>> + Send;
    fn wait_agents_output_with_cancel(
        &self,
        agent_ids: Vec<AgentId>,
        timeout: std::time::Duration,
        cancellation_token: &tokio_util::sync::CancellationToken,
    ) -> impl Future<Output = Result<Value>> + Send;
    fn list_agents(&self) -> impl Future<Output = Vec<AgentSummary>> + Send;
    fn close_agent(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<mai_protocol::AgentStatus>> + Send;
    fn resume_agent(&self, agent_id: AgentId) -> impl Future<Output = Result<AgentSummary>> + Send;
    fn list_mcp_resources(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &tokio_util::sync::CancellationToken,
        server: Option<String>,
        cursor: Option<String>,
    ) -> impl Future<Output = Result<Value>> + Send;
    fn list_mcp_resource_templates(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &tokio_util::sync::CancellationToken,
        server: Option<String>,
        cursor: Option<String>,
    ) -> impl Future<Output = Result<Value>> + Send;
    fn read_mcp_resource(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &tokio_util::sync::CancellationToken,
        server: String,
        uri: String,
    ) -> impl Future<Output = Result<Value>> + Send;
    fn save_task_plan(
        &self,
        agent_id: AgentId,
        title: String,
        markdown: String,
    ) -> impl Future<Output = Result<TaskSummary>> + Send;
    fn submit_review_result(
        &self,
        agent_id: AgentId,
        passed: bool,
        findings: String,
        summary: String,
    ) -> impl Future<Output = Result<TaskReview>> + Send;
    fn save_artifact(
        &self,
        agent_id: AgentId,
        path: String,
        display_name: Option<String>,
    ) -> impl Future<Output = Result<ArtifactInfo>> + Send;
    fn execute_project_github_api_request(
        &self,
        agent: &AgentRecord,
        request: GithubApiRequest,
    ) -> impl Future<Output = Result<ToolExecution>> + Send;
    fn queue_project_review_prs(
        &self,
        agent: &AgentRecord,
        prs: Vec<QueueProjectReviewPr>,
    ) -> impl Future<Output = Result<ToolExecution>> + Send;
    fn execute_project_git_tool(
        &self,
        agent: &AgentRecord,
        name: String,
        arguments: Value,
    ) -> impl Future<Output = Result<ToolExecution>> + Send;
    fn execute_mcp_tool(
        &self,
        agent: &AgentRecord,
        model_name: String,
        arguments: Value,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> impl Future<Output = Result<ToolExecution>> + Send;
}

#[derive(Debug, Clone)]
struct AgentCapability {
    can_spawn_agents: bool,
    can_close_agents: bool,
    communication: AgentCommunicationPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentCommunicationPolicy {
    All,
    ParentAndMaintainer,
}

pub(crate) async fn check_tool_permission(
    state: &RuntimeState,
    agent: &AgentRecord,
    tool_name: &str,
    arguments: &Value,
) -> Result<()> {
    let capability = agent_capability(state, agent).await;
    match route_tool(tool_name) {
        RoutedTool::SpawnAgent if !capability.can_spawn_agents => {
            return Err(RuntimeError::InvalidInput(
                "Tool 'spawn_agent' is not available for worker agents".to_string(),
            ));
        }
        RoutedTool::CloseAgent if !capability.can_close_agents => {
            return Err(RuntimeError::InvalidInput(
                "Tool 'close_agent' is not available for worker agents".to_string(),
            ));
        }
        RoutedTool::SendInput => {
            let target = parse_agent_id(&required_string_argument(arguments, "target")?)?;
            if !agent_can_access_target(state, agent, target).await {
                return Err(RuntimeError::InvalidInput(
                    "target agent is outside this agent's communication policy".to_string(),
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) async fn visible_tool_names(
    state: &RuntimeState,
    agent: &AgentRecord,
    mcp_tools: &[McpTool],
) -> HashSet<String> {
    let capability = agent_capability(state, agent).await;
    let mut names = HashSet::from([
        mai_tools::TOOL_CONTAINER_EXEC.to_string(),
        mai_tools::TOOL_READ_FILE.to_string(),
        mai_tools::TOOL_LIST_FILES.to_string(),
        mai_tools::TOOL_SEARCH_FILES.to_string(),
        mai_tools::TOOL_APPLY_PATCH.to_string(),
        mai_tools::TOOL_CONTAINER_COPY.to_string(),
        mai_tools::TOOL_SEND_INPUT.to_string(),
        mai_tools::TOOL_WAIT_AGENT.to_string(),
        mai_tools::TOOL_LIST_AGENTS.to_string(),
        mai_tools::TOOL_RESUME_AGENT.to_string(),
        mai_tools::TOOL_LIST_MCP_RESOURCES.to_string(),
        mai_tools::TOOL_LIST_MCP_RESOURCE_TEMPLATES.to_string(),
        mai_tools::TOOL_READ_MCP_RESOURCE.to_string(),
        mai_tools::TOOL_SAVE_TASK_PLAN.to_string(),
        mai_tools::TOOL_SUBMIT_REVIEW_RESULT.to_string(),
        mai_tools::TOOL_UPDATE_TODO_LIST.to_string(),
        mai_tools::TOOL_REQUEST_USER_INPUT.to_string(),
        mai_tools::TOOL_SAVE_ARTIFACT.to_string(),
        mai_tools::TOOL_GITHUB_API_REQUEST.to_string(),
        mai_tools::TOOL_GIT_STATUS.to_string(),
        mai_tools::TOOL_GIT_DIFF.to_string(),
        mai_tools::TOOL_GIT_BRANCH.to_string(),
        mai_tools::TOOL_GIT_FETCH.to_string(),
        mai_tools::TOOL_GIT_COMMIT.to_string(),
        mai_tools::TOOL_GIT_PUSH.to_string(),
        mai_tools::TOOL_GIT_WORKSPACE_INFO.to_string(),
        mai_tools::TOOL_GIT_SYNC_DEFAULT_BRANCH.to_string(),
    ]);
    if project_review_queue_tool_visible(agent).await {
        names.insert(mai_tools::TOOL_QUEUE_PROJECT_REVIEW_PRS.to_string());
    }
    if capability.can_spawn_agents {
        names.insert(mai_tools::TOOL_SPAWN_AGENT.to_string());
    }
    if capability.can_close_agents {
        names.insert(mai_tools::TOOL_CLOSE_AGENT.to_string());
    }
    names.extend(mcp_tools.iter().map(|tool| tool.model_name.clone()));
    names
}

pub(crate) async fn execute_tool(
    context: &ToolDispatchContext<'_, impl ToolDispatchOps + ContainerToolOps + ?Sized>,
    agent: &Arc<AgentRecord>,
    agent_id: AgentId,
    turn_id: TurnId,
    name: &str,
    arguments: Value,
    cancellation_token: tokio_util::sync::CancellationToken,
) -> Result<ToolExecution> {
    if cancellation_token.is_cancelled() {
        return Err(RuntimeError::TurnCancelled);
    }
    check_tool_permission(context.state, agent, name, &arguments).await?;
    if let Some(execution) = crate::turn::container::execute_container_tool(
        &context.container,
        agent_id,
        name,
        &arguments,
        &cancellation_token,
    )
    .await?
    {
        return Ok(execution);
    }
    match route_tool(name) {
        RoutedTool::ContainerExec
        | RoutedTool::ContainerCopy
        | RoutedTool::ReadFile
        | RoutedTool::ListFiles
        | RoutedTool::SearchFiles
        | RoutedTool::ApplyPatch => unreachable!("container and file tools are handled above"),
        RoutedTool::SpawnAgent => {
            let request = spawn_agent_tool_request(&arguments)?;
            let result = context.ops.spawn_agent_from_tool(agent_id, request).await?;
            Ok(ToolExecution::new(
                true,
                json!({ "agent": result.agent, "turn_id": result.turn_id }).to_string(),
                false,
            ))
        }
        RoutedTool::SendInput => {
            let target = parse_agent_id(&required_string_argument(&arguments, "target")?)?;
            let collab_input = collab_input_from_args(&arguments)?;
            let message = collab_input.message.ok_or_else(|| {
                RuntimeError::InvalidInput("send_input requires message or text items".to_string())
            })?;
            let interrupt = arguments
                .get("interrupt")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let output = context
                .ops
                .send_input_to_agent(
                    target,
                    None,
                    message,
                    collab_input.skill_mentions,
                    interrupt,
                )
                .await?;
            Ok(ToolExecution::new(true, output.to_string(), false))
        }
        RoutedTool::WaitAgent => {
            let targets = wait_targets(&arguments)?;
            let timeout = wait_timeout(&arguments);
            let output = context
                .ops
                .wait_agents_output_with_cancel(targets, timeout, &cancellation_token)
                .await?;
            Ok(ToolExecution::new(
                true,
                serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string()),
                false,
            ))
        }
        RoutedTool::ListAgents => Ok(ToolExecution::new(
            true,
            serde_json::to_string(&context.ops.list_agents().await)
                .unwrap_or_else(|_| "[]".to_string()),
            false,
        )),
        RoutedTool::CloseAgent => {
            let target = parse_agent_id(&required_string_argument(&arguments, "target")?)?;
            let previous = context.ops.close_agent(target).await?;
            Ok(ToolExecution::new(
                true,
                json!({ "closed": target, "previous_status": previous }).to_string(),
                false,
            ))
        }
        RoutedTool::ResumeAgent => {
            let target = parse_agent_id(&required_string_argument(&arguments, "target")?)?;
            let resumed = context.ops.resume_agent(target).await?;
            Ok(ToolExecution::new(
                true,
                json!({ "agent": resumed }).to_string(),
                false,
            ))
        }
        RoutedTool::ListMcpResources => {
            let output = context
                .ops
                .list_mcp_resources(
                    agent,
                    agent_id,
                    &cancellation_token,
                    optional_string_argument(&arguments, "server"),
                    optional_string_argument(&arguments, "cursor"),
                )
                .await?;
            Ok(ToolExecution::new(true, output.to_string(), false))
        }
        RoutedTool::ListMcpResourceTemplates => {
            let output = context
                .ops
                .list_mcp_resource_templates(
                    agent,
                    agent_id,
                    &cancellation_token,
                    optional_string_argument(&arguments, "server"),
                    optional_string_argument(&arguments, "cursor"),
                )
                .await?;
            Ok(ToolExecution::new(true, output.to_string(), false))
        }
        RoutedTool::ReadMcpResource => {
            let server = required_string_argument(&arguments, "server")?;
            let uri = required_string_argument(&arguments, "uri")?;
            let output = context
                .ops
                .read_mcp_resource(agent, agent_id, &cancellation_token, server, uri)
                .await?;
            Ok(ToolExecution::new(true, output.to_string(), false))
        }
        RoutedTool::SaveTaskPlan => {
            let title = required_string_argument(&arguments, "title")?;
            let markdown = required_string_argument(&arguments, "markdown")?;
            let task = context
                .ops
                .save_task_plan(agent_id, title, markdown)
                .await?;
            Ok(ToolExecution::new(
                true,
                serde_json::to_string(&task).unwrap_or_else(|_| "{}".to_string()),
                false,
            ))
        }
        RoutedTool::SubmitReviewResult => {
            let passed = arguments
                .get("passed")
                .and_then(Value::as_bool)
                .ok_or_else(|| {
                    RuntimeError::InvalidInput("missing boolean field `passed`".to_string())
                })?;
            let findings = required_string_argument(&arguments, "findings")?;
            let summary = required_string_argument(&arguments, "summary")?;
            let review = context
                .ops
                .submit_review_result(agent_id, passed, findings, summary)
                .await?;
            Ok(ToolExecution::new(
                true,
                serde_json::to_string(&review).unwrap_or_else(|_| "{}".to_string()),
                false,
            ))
        }
        RoutedTool::UpdateTodoList => {
            let items = todo_items_from_arguments(&arguments)?;
            context
                .events
                .publish(ServiceEventKind::TodoListUpdated {
                    agent_id,
                    session_id: None,
                    turn_id,
                    items,
                })
                .await;
            Ok(ToolExecution::new(
                true,
                "Todo list updated".to_string(),
                false,
            ))
        }
        RoutedTool::RequestUserInput => {
            let (header, questions) = user_input_questions_from_arguments(&arguments)?;
            context
                .events
                .publish(ServiceEventKind::UserInputRequested {
                    agent_id,
                    session_id: None,
                    turn_id,
                    header,
                    questions,
                })
                .await;
            Ok(ToolExecution::new(
                true,
                "Questions sent to user. Wait for their response in the next message.".to_string(),
                true,
            ))
        }
        RoutedTool::SaveArtifact => {
            let path = required_string_argument(&arguments, "path")?;
            let name = optional_string_argument(&arguments, "name");
            let artifact = context.ops.save_artifact(agent_id, path, name).await?;
            Ok(ToolExecution::new(
                true,
                serde_json::to_string(&artifact).unwrap_or_else(|_| "{}".to_string()),
                false,
            ))
        }
        RoutedTool::GithubApiRequest => {
            let request = github_api_request_from_arguments(&arguments)?;
            context
                .ops
                .execute_project_github_api_request(agent, request)
                .await
        }
        RoutedTool::QueueProjectReviewPrs => {
            let prs = queue_project_review_prs_from_arguments(&arguments)?;
            context.ops.queue_project_review_prs(agent, prs).await
        }
        RoutedTool::GitStatus
        | RoutedTool::GitDiff
        | RoutedTool::GitBranch
        | RoutedTool::GitFetch
        | RoutedTool::GitCommit
        | RoutedTool::GitPush
        | RoutedTool::GitWorkspaceInfo
        | RoutedTool::GitSyncDefaultBranch => {
            context
                .ops
                .execute_project_git_tool(agent, name.to_string(), arguments)
                .await
        }
        RoutedTool::Mcp(model_name) => {
            context
                .ops
                .execute_mcp_tool(agent, model_name, arguments, cancellation_token)
                .await
        }
        RoutedTool::Unknown(name) => Ok(ToolExecution::new(
            false,
            format!("unknown tool: {name}"),
            false,
        )),
    }
}

async fn agent_capability(state: &RuntimeState, agent: &AgentRecord) -> AgentCapability {
    let summary = agent.summary.read().await.clone();
    let is_project_maintainer = if let Some(project_id) = summary.project_id {
        let project = state.projects.read().await.get(&project_id).cloned();
        if let Some(project) = project {
            project.summary.read().await.maintainer_agent_id == summary.id
        } else {
            false
        }
    } else {
        summary.parent_id.is_none()
    };
    if is_project_maintainer || summary.parent_id.is_none() {
        AgentCapability {
            can_spawn_agents: true,
            can_close_agents: true,
            communication: AgentCommunicationPolicy::All,
        }
    } else {
        AgentCapability {
            can_spawn_agents: false,
            can_close_agents: false,
            communication: AgentCommunicationPolicy::ParentAndMaintainer,
        }
    }
}

async fn project_review_queue_tool_visible(agent: &AgentRecord) -> bool {
    let summary = agent.summary.read().await;
    summary.project_id.is_some()
        && matches!(
            summary.role,
            Some(AgentRole::Explorer | AgentRole::Reviewer)
        )
}

async fn agent_can_access_target(
    state: &RuntimeState,
    agent: &AgentRecord,
    target: AgentId,
) -> bool {
    let capability = agent_capability(state, agent).await;
    if capability.communication == AgentCommunicationPolicy::All {
        return true;
    }
    let summary = agent.summary.read().await.clone();
    if summary.parent_id == Some(target) {
        return true;
    }
    let Some(project_id) = summary.project_id else {
        return false;
    };
    let project = state.projects.read().await.get(&project_id).cloned();
    if let Some(project) = project {
        project.summary.read().await.maintainer_agent_id == target
    } else {
        false
    }
}

#[cfg(test)]
pub(crate) struct ToolCallInfo<'a> {
    pub(crate) call_id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) arguments: Value,
}

#[cfg(test)]
pub(crate) struct ToolCallContext<'a> {
    pub(crate) store: &'a ConfigStore,
    pub(crate) events: &'a RuntimeEvents,
    pub(crate) agent: &'a Arc<AgentRecord>,
    pub(crate) agent_id: AgentId,
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
}

#[cfg(test)]
pub(crate) async fn run_tool_call<F, Fut>(
    ctx: &ToolCallContext<'_>,
    call: ToolCallInfo<'_>,
    execute: F,
) -> Result<ToolExecution>
where
    F: FnOnce(Value) -> Fut,
    Fut: Future<Output = Result<ToolExecution>>,
{
    let arguments_preview = trace_preview_value(&call.arguments, 500);
    let inline_arguments = inline_event_arguments(&call.arguments);
    let trace_arguments = call.arguments.clone();
    let started_wall_time = now();
    super::persistence::record_agent_log(
        ctx.store,
        AgentLogRecord {
            agent_id: ctx.agent_id,
            session_id: Some(ctx.session_id),
            turn_id: Some(ctx.turn_id),
            level: "info",
            category: "tool",
            message: "tool started",
            details: json!({
                "call_id": call.call_id,
                "tool_name": call.name,
                "arguments_preview": arguments_preview.as_str(),
            }),
        },
    )
    .await;
    super::persistence::record_tool_trace_started(
        ctx.store,
        ToolTraceDetail {
            agent_id: ctx.agent_id,
            session_id: Some(ctx.session_id),
            turn_id: Some(ctx.turn_id),
            call_id: call.call_id.to_string(),
            tool_name: call.name.to_string(),
            arguments: trace_arguments.clone(),
            output: String::new(),
            success: false,
            duration_ms: None,
            started_at: Some(started_wall_time),
            completed_at: None,
            output_preview: String::new(),
            output_artifacts: Vec::new(),
        },
        started_wall_time,
    )
    .await;
    ctx.events
        .publish(ServiceEventKind::ToolStarted {
            agent_id: ctx.agent_id,
            session_id: Some(ctx.session_id),
            turn_id: ctx.turn_id,
            call_id: call.call_id.to_string(),
            tool_name: call.name.to_string(),
            arguments_preview: Some(arguments_preview),
            arguments: inline_arguments,
        })
        .await;

    let started_at = Instant::now();
    let raw_arguments = call.arguments.to_string();
    let output = execute(call.arguments).await;
    let duration_ms = u128_to_u64(started_at.elapsed().as_millis());
    let execution = match output {
        Ok(execution) => execution,
        Err(RuntimeError::TurnCancelled) => return Err(RuntimeError::TurnCancelled),
        Err(err) => ToolExecution::new(false, err.to_string(), false),
    };

    super::history::record_history_item(
        ctx.store,
        ctx.agent,
        ctx.agent_id,
        ctx.session_id,
        super::history::tool_result_message(
            call.call_id.to_string(),
            call.name.to_string(),
            raw_arguments,
            execution.model_output.clone(),
        ),
    )
    .await?;

    let completed_wall_time = now();
    let output_preview = trace_preview_output(&execution.output, 500);
    super::persistence::record_tool_trace_completed(
        ctx.store,
        ToolTraceDetail {
            agent_id: ctx.agent_id,
            session_id: Some(ctx.session_id),
            turn_id: Some(ctx.turn_id),
            call_id: call.call_id.to_string(),
            tool_name: call.name.to_string(),
            arguments: trace_arguments,
            output: execution.output.clone(),
            success: execution.success,
            duration_ms: Some(duration_ms),
            started_at: Some(started_wall_time),
            completed_at: Some(completed_wall_time),
            output_preview: output_preview.clone(),
            output_artifacts: execution.output_artifacts.clone(),
        },
        started_wall_time,
        completed_wall_time,
    )
    .await;
    super::persistence::record_agent_log(
        ctx.store,
        AgentLogRecord {
            agent_id: ctx.agent_id,
            session_id: Some(ctx.session_id),
            turn_id: Some(ctx.turn_id),
            level: if execution.success { "info" } else { "warn" },
            category: "tool",
            message: "tool completed",
            details: json!({
                "call_id": call.call_id,
                "tool_name": call.name,
                "success": execution.success,
                "duration_ms": duration_ms,
                "output_preview": output_preview.as_str(),
            }),
        },
    )
    .await;
    ctx.events
        .publish(ServiceEventKind::ToolCompleted {
            agent_id: ctx.agent_id,
            session_id: Some(ctx.session_id),
            turn_id: ctx.turn_id,
            call_id: call.call_id.to_string(),
            tool_name: call.name.to_string(),
            success: execution.success,
            output_preview,
            duration_ms: Some(duration_ms),
        })
        .await;

    Ok(execution)
}

pub(crate) async fn prepare_tool_output_capture(
    artifact_files_root: &Path,
    agent_id: AgentId,
    command: &str,
) -> Result<ToolOutputCapture> {
    let call_id = Uuid::new_v4().to_string();
    let stdout_id = Uuid::new_v4().to_string();
    let stderr_id = Uuid::new_v4().to_string();
    let stdout_name = tool_output_file_name(command, "stdout");
    let stderr_name = tool_output_file_name(command, "stderr");
    let stdout_path = tool_output_artifact_file_path(
        artifact_files_root,
        agent_id,
        &call_id,
        &stdout_id,
        &stdout_name,
    );
    let stderr_path = tool_output_artifact_file_path(
        artifact_files_root,
        agent_id,
        &call_id,
        &stderr_id,
        &stderr_name,
    );
    if let Some(parent) = stdout_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if let Some(parent) = stderr_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    Ok(ToolOutputCapture {
        call_id,
        stdout_id,
        stderr_id,
        stdout_path,
        stderr_path,
        stdout_name,
        stderr_name,
    })
}

pub(crate) async fn tool_output_artifacts_from_capture(
    agent_id: AgentId,
    capture: &ToolOutputCapture,
    stdout_bytes: u64,
    stderr_bytes: u64,
) -> Result<Vec<ToolOutputArtifactInfo>> {
    let created_at = now();
    let mut artifacts = Vec::new();
    if stdout_bytes > 0 {
        artifacts.push(ToolOutputArtifactInfo {
            id: capture.stdout_id.clone(),
            call_id: capture.call_id.clone(),
            agent_id,
            name: capture.stdout_name.clone(),
            stream: "stdout".to_string(),
            size_bytes: stdout_bytes,
            created_at,
        });
    } else {
        let _ = tokio::fs::remove_file(&capture.stdout_path).await;
    }
    if stderr_bytes > 0 {
        artifacts.push(ToolOutputArtifactInfo {
            id: capture.stderr_id.clone(),
            call_id: capture.call_id.clone(),
            agent_id,
            name: capture.stderr_name.clone(),
            stream: "stderr".to_string(),
            size_bytes: stderr_bytes,
            created_at,
        });
    } else {
        let _ = tokio::fs::remove_file(&capture.stderr_path).await;
    }
    Ok(artifacts)
}

pub(crate) fn tool_output_artifact_file_path(
    artifact_files_root: &Path,
    agent_id: AgentId,
    call_id: &str,
    artifact_id: &str,
    name: &str,
) -> PathBuf {
    tool_output_artifact_dir(artifact_files_root, agent_id, call_id, artifact_id).join(name)
}

pub(crate) fn trace_preview_value(value: &Value, max: usize) -> String {
    let redacted = redacted_preview_value(value);
    let serialized =
        serde_json::to_string_pretty(&redacted).unwrap_or_else(|_| redacted.to_string());
    preview(&serialized, max)
}

pub(crate) fn trace_preview_output(output: &str, max: usize) -> String {
    serde_json::from_str::<Value>(output)
        .map(|value| trace_preview_value(&value, max))
        .unwrap_or_else(|_| preview(&redact_preview_string(output), max))
}

fn required_string_argument(arguments: &Value, field: &str) -> Result<String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| RuntimeError::InvalidInput(format!("missing string field `{field}`")))
}

fn optional_string_argument(arguments: &Value, field: &str) -> Option<String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn queue_project_review_prs_from_arguments(arguments: &Value) -> Result<Vec<QueueProjectReviewPr>> {
    let Some(items) = arguments.get("prs").and_then(Value::as_array) else {
        return Err(RuntimeError::InvalidInput(
            "missing array field `prs`".to_string(),
        ));
    };
    items
        .iter()
        .map(|item| {
            let number = item.get("number").and_then(Value::as_u64).ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "each `prs` item must include integer field `number`".to_string(),
                )
            })?;
            Ok(QueueProjectReviewPr {
                number,
                head_sha: optional_string_argument(item, "head_sha"),
                reason: optional_string_argument(item, "reason"),
            })
        })
        .collect()
}

fn github_api_request_from_arguments(arguments: &Value) -> Result<GithubApiRequest> {
    Ok(GithubApiRequest {
        method: required_string_argument(arguments, "method")?,
        path: required_string_argument(arguments, "path")?,
        body: optional_json_body_argument(arguments, "body")?,
    })
}

fn optional_json_body_argument(arguments: &Value, field: &str) -> Result<Option<Value>> {
    let Some(value) = arguments.get(field) else {
        return Ok(None);
    };
    let parsed = if let Some(raw) = value.as_str() {
        serde_json::from_str(raw).map_err(|err| {
            RuntimeError::InvalidInput(format!("field `{field}` must be JSON: {err}"))
        })?
    } else {
        value.clone()
    };
    if parsed.is_object() || parsed.is_null() {
        return Ok(Some(parsed));
    }
    Err(RuntimeError::InvalidInput(format!(
        "field `{field}` must be a JSON object or null"
    )))
}

fn parse_agent_id(value: &str) -> Result<AgentId> {
    Uuid::parse_str(value)
        .map_err(|err| RuntimeError::InvalidInput(format!("invalid agent_id `{value}`: {err}")))
}

fn spawn_agent_tool_request(arguments: &Value) -> Result<SpawnAgentToolRequest> {
    let agent_type = optional_string_argument(arguments, "agentType");
    let role = agent_type
        .as_deref()
        .and_then(agents::agent_type_role)
        .unwrap_or_default();
    let role_profile_requested = agent_type.as_deref().is_some_and(|value| {
        matches!(
            value.trim().to_lowercase().as_str(),
            "planner" | "explorer" | "executor" | "reviewer"
        )
    });
    Ok(SpawnAgentToolRequest {
        name: optional_string_argument(arguments, "taskName"),
        role,
        legacy_role: role_profile_requested.then_some(role),
        model: optional_string_argument(arguments, "model"),
        reasoning_effort: optional_string_argument(arguments, "reasoningEffort"),
        fork_context: arguments
            .get("forkTurns")
            .and_then(Value::as_u64)
            .is_some_and(|turns| turns > 0),
        collab_input: collab_input_from_args(arguments)?,
    })
}

fn todo_items_from_arguments(arguments: &Value) -> Result<Vec<TodoItem>> {
    let Some(items_arg) = arguments.get("items") else {
        return Err(RuntimeError::InvalidInput(
            "missing field `items`".to_string(),
        ));
    };
    let items_value = if let Some(raw) = items_arg.as_str() {
        serde_json::from_str(raw)
            .map_err(|e| RuntimeError::InvalidInput(format!("invalid items JSON string: {e}")))?
    } else {
        items_arg.clone()
    };
    let items = items_value
        .as_array()
        .ok_or_else(|| RuntimeError::InvalidInput("items must be an array".to_string()))?;
    items
        .iter()
        .map(|item| {
            let step = required_string_argument(item, "step")?;
            let status = match required_string_argument(item, "status")?.as_str() {
                "pending" => mai_protocol::TodoListStatus::Pending,
                "inProgress" => mai_protocol::TodoListStatus::InProgress,
                "completed" => mai_protocol::TodoListStatus::Completed,
                other => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "invalid todo status `{other}`"
                    )));
                }
            };
            Ok(TodoItem { step, status })
        })
        .collect()
}

fn collab_input_from_args(arguments: &Value) -> Result<CollabInput> {
    let mut input = CollabInput::default();
    if let Some(message) = optional_string_argument(arguments, "message") {
        input.message = Some(message);
    }
    let Some(items) = arguments.get("items").and_then(Value::as_array) else {
        return Ok(input);
    };
    let mut parts = Vec::new();
    for item in items {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("text");
        match item_type {
            "text" => {
                let text = item.get("text").and_then(Value::as_str).ok_or_else(|| {
                    RuntimeError::InvalidInput("text collab items require `text`".to_string())
                })?;
                parts.push(text.to_string());
            }
            "skill" => {
                let mention = item
                    .get("path")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("name").and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput(
                            "skill collab items require `name` or `path`".to_string(),
                        )
                    })?;
                input.skill_mentions.push(mention.to_string());
            }
            _ => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unsupported collab item type `{item_type}`; expected text or skill"
                )));
            }
        }
    }
    if !parts.is_empty() {
        input.message = Some(match input.message {
            Some(existing) if !existing.is_empty() => format!("{existing}\n{}", parts.join("\n")),
            _ => parts.join("\n"),
        });
    }
    Ok(input)
}

fn wait_targets(arguments: &Value) -> Result<Vec<AgentId>> {
    if let Some(targets) = arguments.get("targets").and_then(Value::as_array) {
        if targets.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "targets must be non-empty".to_string(),
            ));
        }
        return targets
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput("targets must contain strings".to_string())
                    })
                    .and_then(parse_agent_id)
            })
            .collect();
    }
    Ok(vec![parse_agent_id(&required_string_argument(
        arguments, "target",
    )?)?])
}

fn wait_timeout(arguments: &Value) -> std::time::Duration {
    if let Some(ms) = arguments.get("timeoutMs").and_then(Value::as_u64) {
        return std::time::Duration::from_millis(ms);
    }
    std::time::Duration::from_secs(DEFAULT_WAIT_AGENT_OBSERVATION_SECS)
}

fn user_input_questions_from_arguments(
    arguments: &Value,
) -> Result<(String, Vec<UserInputQuestion>)> {
    let questions_arg = arguments
        .get("questions")
        .ok_or_else(|| RuntimeError::InvalidInput("missing field `questions`".to_string()))?;
    let raw_questions: Vec<Value> = serde_json::from_value(questions_arg.clone())
        .map_err(|e| RuntimeError::InvalidInput(format!("invalid questions: {e}")))?;
    let mut questions = Vec::with_capacity(raw_questions.len());
    let mut header = None;
    for raw in &raw_questions {
        if header.is_none() {
            header = raw
                .get("header")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        let id = raw
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let question = raw
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let options_raw = raw
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut options = Vec::with_capacity(options_raw.len());
        for opt in &options_raw {
            let label = opt
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let description = opt
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string);
            options.push(UserInputOption { label, description });
        }
        questions.push(UserInputQuestion {
            id,
            question,
            options,
        });
    }
    Ok((header.unwrap_or_else(|| "Input".to_string()), questions))
}

fn bounded_model_tool_output_with_tokens(output: &str, max_output_tokens: usize) -> String {
    let max_bytes = max_output_tokens * TOKEN_ESTIMATE_BYTES;
    if output.len() <= max_bytes {
        return output.to_string();
    }
    if let Ok(value) = serde_json::from_str::<Value>(output) {
        return bounded_json_tool_output(value, max_bytes).to_string();
    }
    let (text, truncated, bytes_omitted, next_offset) = bounded_text(output, max_bytes, 0);
    json!({
        "truncated": truncated,
        "bytesReturned": text.len(),
        "bytesOmitted": bytes_omitted,
        "nextOffset": next_offset,
        "text": text,
    })
    .to_string()
}

fn bounded_json_tool_output(mut value: Value, max_bytes: usize) -> Value {
    match &mut value {
        Value::Object(map) => {
            for key in [
                "stdout",
                "stderr",
                "body",
                "text",
                "tarBase64",
                "contentBase64",
            ] {
                if let Some(Value::String(text)) = map.get_mut(key) {
                    let (bounded, truncated, bytes_omitted, next_offset) =
                        bounded_text(text, max_bytes, 0);
                    if truncated {
                        *text = bounded;
                        map.insert("truncated".to_string(), Value::Bool(true));
                        map.insert("bytesReturned".to_string(), json!(max_bytes));
                        map.insert("bytesOmitted".to_string(), json!(bytes_omitted));
                        map.insert("nextOffset".to_string(), json!(next_offset));
                        break;
                    }
                }
            }
            if value.to_string().len() > max_bytes {
                let serialized = value.to_string();
                let (text, _, bytes_omitted, next_offset) = bounded_text(&serialized, max_bytes, 0);
                json!({
                    "truncated": true,
                    "bytesReturned": text.len(),
                    "bytesOmitted": bytes_omitted,
                    "nextOffset": next_offset,
                    "jsonPreview": text,
                })
            } else {
                value
            }
        }
        _ => {
            let serialized = value.to_string();
            if serialized.len() <= max_bytes {
                value
            } else {
                let (text, _, bytes_omitted, next_offset) = bounded_text(&serialized, max_bytes, 0);
                json!({
                    "truncated": true,
                    "bytesReturned": text.len(),
                    "bytesOmitted": bytes_omitted,
                    "nextOffset": next_offset,
                    "jsonPreview": text,
                })
            }
        }
    }
}

fn bounded_text(
    value: &str,
    max_bytes: usize,
    offset: usize,
) -> (String, bool, usize, Option<usize>) {
    if value.len() <= max_bytes {
        return (value.to_string(), false, 0, None);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let text = value[..end].to_string();
    let omitted = value.len().saturating_sub(end);
    (text, true, omitted, Some(offset.saturating_add(end)))
}

#[cfg(test)]
fn inline_event_arguments(value: &Value) -> Option<Value> {
    let redacted = redacted_preview_value(value);
    let serialized = serde_json::to_string(&redacted).ok()?;
    (serialized.len() <= 2_000).then_some(redacted)
}

fn redacted_preview_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                if is_sensitive_key(key) {
                    out.insert(key.clone(), Value::String("<redacted>".to_string()));
                } else {
                    out.insert(key.clone(), redacted_preview_value(value));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(20)
                .map(redacted_preview_value)
                .chain(
                    (items.len() > 20)
                        .then(|| Value::String(format!("<{} more items>", items.len() - 20))),
                )
                .collect(),
        ),
        Value::String(value) => Value::String(redact_preview_string(value)),
        _ => value.clone(),
    }
}

fn redact_preview_string(value: &str) -> String {
    if value.len() > 240 && looks_like_base64(value) {
        return format!("<base64 elided: {} chars>", value.len());
    }
    if value.len() > 800 {
        return format!("{}...", value.chars().take(800).collect::<String>());
    }
    value.to_string()
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("token")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("authorization")
        || key.contains("api_key")
        || key.ends_with("_key")
        || key.contains("base64")
}

fn looks_like_base64(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.len() > 240
        && trimmed.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'\n' | b'\r')
        })
}

fn tool_output_artifact_dir(
    artifact_files_root: &Path,
    agent_id: AgentId,
    call_id: &str,
    artifact_id: &str,
) -> PathBuf {
    artifact_files_root
        .join("tool-output")
        .join(agent_id.to_string())
        .join(safe_path_component(call_id))
        .join(artifact_id)
}

fn tool_output_file_name(command: &str, stream: &str) -> String {
    let command = command
        .split_whitespace()
        .next()
        .map(safe_path_component)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "command".to_string());
    format!("{command}-{stream}.txt")
}

fn safe_path_component(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('.')
        .trim_matches('_')
        .to_string()
}

#[cfg(test)]
fn u128_to_u64(value: u128) -> u64 {
    value.min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{AgentSessionSummary, AgentStatus, AgentSummary, now};
    use mai_store::ToolTraceFilter;
    use pl_protocol::{MessageContent, ToolResultMetadata};
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;
    use tokio::sync::{Mutex, RwLock};
    use tokio_util::sync::CancellationToken;

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
                name: "tool-test-agent".to_string(),
                status: AgentStatus::WaitingTool,
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
                token_usage: Default::default(),
            };
            let session_summary = AgentSessionSummary {
                id: session_id,
                title: "Default".to_string(),
                created_at,
                updated_at: created_at,
                message_count: 0,
                token_usage: Default::default(),
            };
            let agent = Arc::new(AgentRecord {
                summary: RwLock::new(summary.clone()),
                sessions: Mutex::new(vec![AgentSessionRecord {
                    summary: session_summary.clone(),
                    messages: Vec::new(),
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

        async fn history(&self) -> Vec<pl_protocol::Message> {
            self.store
                .load_agent_history(self.agent_id, self.session_id)
                .await
                .expect("load history")
        }
    }

    #[tokio::test]
    async fn tool_lifecycle_records_trace_event_and_function_call_output() {
        let harness = Harness::new().await;
        let execution = run_tool_call(
            &ToolCallContext {
                store: harness.store.as_ref(),
                events: &harness.events,
                agent: &harness.agent,
                agent_id: harness.agent_id,
                session_id: harness.session_id,
                turn_id: harness.turn_id,
            },
            ToolCallInfo {
                call_id: "call_1",
                name: "demo_tool",
                arguments: json!({ "token": "secret", "path": "src/lib.rs" }),
            },
            |_| async {
                Ok(ToolExecution::new(
                    true,
                    json!({ "ok": true, "token": "secret-output" }).to_string(),
                    false,
                ))
            },
        )
        .await
        .expect("tool call");

        assert!(execution.success);
        let history = harness.history().await;
        assert_eq!(history.len(), 1);
        let metadata =
            ToolResultMetadata::from_metadata(&history[0].metadata).expect("tool result metadata");
        assert_eq!(metadata.tool_call_id, "call_1");
        assert!(matches!(
            &history[0].content,
            MessageContent::Text(output) if output.contains("\"ok\":true")
        ));
        let events = harness.events.snapshot().await;
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            ServiceEventKind::ToolStarted {
                call_id,
                arguments: Some(arguments),
                ..
            } if call_id == "call_1"
                && arguments["token"] == "<redacted>"
                && arguments["path"] == "src/lib.rs"
        )));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            ServiceEventKind::ToolCompleted {
                call_id,
                success: true,
                output_preview,
                ..
            } if call_id == "call_1"
                && output_preview.contains("<redacted>")
                && !output_preview.contains("secret-output")
        )));
        let traces = harness
            .store
            .list_tool_traces(
                harness.agent_id,
                ToolTraceFilter {
                    session_id: Some(harness.session_id),
                    turn_id: Some(harness.turn_id),
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .expect("traces");
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].call_id, "call_1");
        assert!(traces[0].success);
    }

    #[tokio::test]
    async fn tool_lifecycle_turn_cancelled_error_is_not_converted_to_tool_output() {
        let harness = Harness::new().await;
        let err = run_tool_call(
            &ToolCallContext {
                store: harness.store.as_ref(),
                events: &harness.events,
                agent: &harness.agent,
                agent_id: harness.agent_id,
                session_id: harness.session_id,
                turn_id: harness.turn_id,
            },
            ToolCallInfo {
                call_id: "call_1",
                name: "demo_tool",
                arguments: json!({}),
            },
            |_| async { Err(RuntimeError::TurnCancelled) },
        )
        .await
        .expect_err("cancel should propagate");

        assert!(matches!(err, RuntimeError::TurnCancelled));
        assert!(harness.history().await.is_empty());
        assert!(
            !harness
                .events
                .snapshot()
                .await
                .iter()
                .any(|event| matches!(event.kind, ServiceEventKind::ToolCompleted { .. }))
        );
    }

    #[tokio::test]
    async fn capture_artifacts_keep_non_empty_streams_and_delete_empty_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent_id = Uuid::new_v4();
        let capture = prepare_tool_output_capture(dir.path(), agent_id, "cargo test")
            .await
            .expect("capture");
        tokio::fs::write(&capture.stdout_path, b"ok")
            .await
            .expect("stdout");
        tokio::fs::write(&capture.stderr_path, b"")
            .await
            .expect("stderr");

        let artifacts = tool_output_artifacts_from_capture(agent_id, &capture, 2, 0)
            .await
            .expect("artifacts");

        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].stream, "stdout");
        assert_eq!(artifacts[0].name, "cargo-stdout.txt");
        assert!(capture.stdout_path.exists());
        assert!(!capture.stderr_path.exists());
    }

    #[test]
    fn trace_preview_redacts_sensitive_values() {
        let value = json!({
            "token": "secret-token",
            "nested": { "api_key": "secret-key", "normal": "visible" },
        });
        let preview = trace_preview_value(&value, 1_000);

        assert!(preview.contains("<redacted>"));
        assert!(preview.contains("visible"));
        assert!(!preview.contains("secret-token"));
        assert!(!preview.contains("secret-key"));
    }

    #[test]
    fn github_api_request_from_arguments_parses_json_string_body() {
        let request = github_api_request_from_arguments(&json!({
            "method": "POST",
            "path": "/repos/owner/repo/pulls/42/reviews",
            "body": r#"{"event":"COMMENT","body":"Looks good."}"#
        }))
        .expect("request");

        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/repos/owner/repo/pulls/42/reviews");
        assert_eq!(
            request.body,
            Some(json!({
                "event": "COMMENT",
                "body": "Looks good."
            }))
        );
    }

    #[test]
    fn github_api_request_from_arguments_rejects_non_object_body() {
        let err = github_api_request_from_arguments(&json!({
            "method": "POST",
            "path": "/repos/owner/repo/issues/42/comments",
            "body": "[\"not\", \"an\", \"object\"]"
        }))
        .expect_err("body array should be rejected");

        assert!(
            err.to_string()
                .contains("field `body` must be a JSON object or null")
        );
    }

    #[test]
    fn tool_output_is_truncated_for_model_history_but_trace_keeps_full_output() {
        let long_stdout = "x".repeat(DEFAULT_MODEL_TOOL_OUTPUT_BYTES + 100);
        let execution = ToolExecution::new(
            true,
            json!({ "status": 0, "stdout": long_stdout, "stderr": "" }).to_string(),
            false,
        );

        assert!(execution.output.len() > execution.model_output.len());
        assert!(execution.output.contains(&"x".repeat(100)));
        let model_value =
            serde_json::from_str::<Value>(&execution.model_output).expect("model output json");
        assert_eq!(model_value["truncated"], true);
        let visible = model_value
            .get("stdout")
            .or_else(|| model_value.get("jsonPreview"))
            .and_then(Value::as_str)
            .expect("visible output");
        assert!(model_value.get("bytesReturned").is_some());
        assert!(model_value.get("bytes_returned").is_none());
        assert!(visible.len() <= DEFAULT_MODEL_TOOL_OUTPUT_BYTES);
    }
}
