use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use mai_docker::ExecCaptureOptions;
use mai_mcp::McpTool;
use mai_protocol::{
    AgentId, AgentRole, AgentSummary, ArtifactInfo, ModelInputItem, ServiceEventKind, SessionId,
    TaskReview, TaskSummary, TodoItem, ToolOutputArtifactInfo, ToolTraceDetail, TurnId,
    UserInputOption, UserInputQuestion, now, preview,
};
use mai_store::ConfigStore;
use mai_tools::{RoutedTool, route_tool};
use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use crate::agents;
use crate::events::RuntimeEvents;
use crate::state::{AgentRecord, CollabInput, RuntimeState};
use crate::tools::files::ContainerFileTools;
use crate::{Result, RuntimeError};

const TOKEN_ESTIMATE_BYTES: usize = 4;
pub(crate) const DEFAULT_MODEL_TOOL_OUTPUT_TOKENS: usize = 10_000;
const DEFAULT_EXEC_OUTPUT_BYTES_CAP: usize = 1024 * 1024;
const MAX_EXEC_OUTPUT_BYTES_CAP: usize = 16 * 1024 * 1024;
const MAX_MODEL_TOOL_OUTPUT_TOKENS: usize = 100_000;
const DEFAULT_WAIT_AGENT_OBSERVATION_SECS: u64 = 30;
#[cfg(test)]
const DEFAULT_MODEL_TOOL_OUTPUT_BYTES: usize =
    DEFAULT_MODEL_TOOL_OUTPUT_TOKENS * TOKEN_ESTIMATE_BYTES;

#[derive(Debug)]
pub(crate) struct ToolExecution {
    pub(crate) success: bool,
    pub(crate) output: String,
    pub(crate) model_output: String,
    pub(crate) ends_turn: bool,
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

pub(crate) struct ContainerToolContext<'a> {
    pub(crate) docker: &'a mai_docker::DockerClient,
    pub(crate) artifact_files_root: &'a Path,
    pub(crate) ops: &'a dyn ContainerToolOps,
}

#[async_trait::async_trait]
pub(crate) trait ContainerToolOps: Send + Sync {
    async fn container_id(&self, agent_id: AgentId) -> Result<String>;
}

pub(crate) struct ToolDispatchContext<'a> {
    pub(crate) state: &'a RuntimeState,
    pub(crate) container: ContainerToolContext<'a>,
    pub(crate) events: &'a RuntimeEvents,
    pub(crate) ops: &'a dyn ToolDispatchOps,
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

#[async_trait::async_trait]
pub(crate) trait ToolDispatchOps: Send + Sync {
    async fn spawn_agent_from_tool(
        &self,
        parent_agent_id: AgentId,
        request: SpawnAgentToolRequest,
    ) -> Result<SpawnAgentToolResult>;
    async fn send_input_to_agent(
        &self,
        target: AgentId,
        session_id: Option<SessionId>,
        message: String,
        skill_mentions: Vec<String>,
        interrupt: bool,
    ) -> Result<Value>;
    async fn wait_agents_output_with_cancel(
        &self,
        agent_ids: Vec<AgentId>,
        timeout: std::time::Duration,
        cancellation_token: &tokio_util::sync::CancellationToken,
    ) -> Result<Value>;
    async fn list_agents(&self) -> Vec<AgentSummary>;
    async fn close_agent(&self, agent_id: AgentId) -> Result<mai_protocol::AgentStatus>;
    async fn resume_agent(&self, agent_id: AgentId) -> Result<AgentSummary>;
    async fn list_mcp_resources(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &tokio_util::sync::CancellationToken,
        server: Option<String>,
        cursor: Option<String>,
    ) -> Result<Value>;
    async fn list_mcp_resource_templates(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &tokio_util::sync::CancellationToken,
        server: Option<String>,
        cursor: Option<String>,
    ) -> Result<Value>;
    async fn read_mcp_resource(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &tokio_util::sync::CancellationToken,
        server: String,
        uri: String,
    ) -> Result<Value>;
    async fn save_task_plan(
        &self,
        agent_id: AgentId,
        title: String,
        markdown: String,
    ) -> Result<TaskSummary>;
    async fn submit_review_result(
        &self,
        agent_id: AgentId,
        passed: bool,
        findings: String,
        summary: String,
    ) -> Result<TaskReview>;
    async fn save_artifact(
        &self,
        agent_id: AgentId,
        path: String,
        display_name: Option<String>,
    ) -> Result<ArtifactInfo>;
    async fn execute_project_github_api_get(
        &self,
        agent: &AgentRecord,
        path: String,
    ) -> Result<ToolExecution>;
    async fn execute_mcp_tool(
        &self,
        agent: &AgentRecord,
        model_name: String,
        arguments: Value,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<ToolExecution>;
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
        RoutedTool::SendInput | RoutedTool::SendMessage => {
            let target = match route_tool(tool_name) {
                RoutedTool::SendInput => parse_agent_id(&required_any_string_argument(
                    arguments,
                    &["target", "agent_id"],
                )?)?,
                RoutedTool::SendMessage => {
                    parse_agent_id(&required_string_argument(arguments, "agent_id")?)?
                }
                _ => unreachable!(),
            };
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
        mai_tools::TOOL_CONTAINER_CP_UPLOAD.to_string(),
        mai_tools::TOOL_CONTAINER_CP_DOWNLOAD.to_string(),
        mai_tools::TOOL_SEND_INPUT.to_string(),
        mai_tools::TOOL_SEND_MESSAGE.to_string(),
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
        mai_tools::TOOL_GITHUB_API_GET.to_string(),
    ]);
    if capability.can_spawn_agents {
        names.insert(mai_tools::TOOL_SPAWN_AGENT.to_string());
    }
    if capability.can_close_agents {
        names.insert(mai_tools::TOOL_CLOSE_AGENT.to_string());
    }
    names.extend(mcp_tools.iter().map(|tool| tool.model_name.clone()));
    names
}

pub(crate) async fn execute_container_tool(
    context: &ContainerToolContext<'_>,
    agent_id: AgentId,
    name: &str,
    arguments: &Value,
    cancellation_token: &tokio_util::sync::CancellationToken,
) -> Result<Option<ToolExecution>> {
    match route_tool(name) {
        RoutedTool::ContainerExec => {
            let command = required_string_argument(arguments, "command")?;
            let cwd = optional_string_argument(arguments, "cwd");
            let timeout = arguments.get("timeout_secs").and_then(Value::as_u64);
            let max_output_tokens = optional_usize_argument(arguments, "max_output_tokens")?
                .unwrap_or(DEFAULT_MODEL_TOOL_OUTPUT_TOKENS)
                .clamp(1, MAX_MODEL_TOOL_OUTPUT_TOKENS);
            let output_bytes_cap = optional_usize_argument(arguments, "output_bytes_cap")?
                .unwrap_or(DEFAULT_EXEC_OUTPUT_BYTES_CAP)
                .clamp(1, MAX_EXEC_OUTPUT_BYTES_CAP);
            let container_id = context.ops.container_id(agent_id).await?;
            let capture =
                prepare_tool_output_capture(context.artifact_files_root, agent_id, &command)
                    .await?;
            let output = context
                .docker
                .exec_shell_captured_with_cancel(
                    &container_id,
                    &command,
                    cwd.as_deref(),
                    timeout,
                    ExecCaptureOptions {
                        stdout_path: &capture.stdout_path,
                        stderr_path: &capture.stderr_path,
                        output_bytes_cap,
                    },
                    cancellation_token,
                )
                .await?;
            let artifacts = tool_output_artifacts_from_capture(
                agent_id,
                &capture,
                output.stdout_bytes,
                output.stderr_bytes,
            )
            .await?;
            Ok(Some(ToolExecution::with_model_tokens(
                output.output.status == 0,
                serde_json::to_string(&json!({
                    "status": output.output.status,
                    "stdout": output.output.stdout,
                    "stderr": output.output.stderr,
                    "stdout_truncated": output.stdout_truncated,
                    "stderr_truncated": output.stderr_truncated,
                    "stdout_bytes": output.stdout_bytes,
                    "stderr_bytes": output.stderr_bytes,
                    "output_artifacts": artifacts,
                }))
                .unwrap_or_else(|_| "{}".to_string()),
                false,
                max_output_tokens,
                artifacts,
            )))
        }
        RoutedTool::ReadFile => {
            let container_id = context.ops.container_id(agent_id).await?;
            let output = ContainerFileTools::new(context.docker, &container_id)
                .read_file(arguments)
                .await?;
            Ok(Some(ToolExecution::new(true, output.to_string(), false)))
        }
        RoutedTool::ListFiles => {
            let container_id = context.ops.container_id(agent_id).await?;
            let output = ContainerFileTools::new(context.docker, &container_id)
                .list_files(arguments)
                .await?;
            Ok(Some(ToolExecution::new(true, output.to_string(), false)))
        }
        RoutedTool::SearchFiles => {
            let container_id = context.ops.container_id(agent_id).await?;
            let output = ContainerFileTools::new(context.docker, &container_id)
                .search_files(arguments, cancellation_token)
                .await?;
            Ok(Some(ToolExecution::new(true, output.to_string(), false)))
        }
        RoutedTool::ApplyPatch => {
            let container_id = context.ops.container_id(agent_id).await?;
            let output = ContainerFileTools::new(context.docker, &container_id)
                .apply_patch(arguments)
                .await?;
            Ok(Some(ToolExecution::new(true, output.to_string(), false)))
        }
        RoutedTool::ContainerCpUpload => {
            let path = required_string_argument(arguments, "path")?;
            let content_base64 = required_string_argument(arguments, "content_base64")?;
            let bytes = upload_file(context, agent_id, &path, &content_base64).await?;
            Ok(Some(ToolExecution::new(
                true,
                json!({ "path": path, "bytes": bytes }).to_string(),
                false,
            )))
        }
        RoutedTool::ContainerCpDownload => {
            let path = required_string_argument(arguments, "path")?;
            let bytes = download_file_tar(context, agent_id, &path).await?;
            Ok(Some(ToolExecution::new(
                true,
                json!({
                    "path": path,
                    "tar_base64": BASE64.encode(bytes),
                })
                .to_string(),
                false,
            )))
        }
        _ => Ok(None),
    }
}

pub(crate) async fn execute_tool(
    context: &ToolDispatchContext<'_>,
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
    if let Some(execution) = execute_container_tool(
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
        | RoutedTool::ReadFile
        | RoutedTool::ListFiles
        | RoutedTool::SearchFiles
        | RoutedTool::ApplyPatch
        | RoutedTool::ContainerCpUpload
        | RoutedTool::ContainerCpDownload => unreachable!("container tools are handled above"),
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
            let target = parse_agent_id(&required_any_string_argument(
                &arguments,
                &["target", "agent_id"],
            )?)?;
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
        RoutedTool::SendMessage => {
            let target = parse_agent_id(&required_string_argument(&arguments, "agent_id")?)?;
            let session_id = optional_string_argument(&arguments, "session_id")
                .as_deref()
                .map(parse_session_id)
                .transpose()?;
            let message = required_string_argument(&arguments, "message")?;
            let output = context
                .ops
                .send_input_to_agent(target, session_id, message, Vec::new(), false)
                .await?;
            Ok(ToolExecution::new(true, output.to_string(), false))
        }
        RoutedTool::WaitAgent => {
            let legacy_single_target =
                arguments.get("targets").is_none() && arguments.get("agent_id").is_some();
            let targets = wait_targets(&arguments)?;
            let timeout = wait_timeout(&arguments);
            let output = context
                .ops
                .wait_agents_output_with_cancel(targets, timeout, &cancellation_token)
                .await?;
            if legacy_single_target {
                return Ok(ToolExecution::new(
                    true,
                    serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string()),
                    false,
                ));
            }
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
            let target = parse_agent_id(&required_any_string_argument(
                &arguments,
                &["target", "agent_id"],
            )?)?;
            let previous = context.ops.close_agent(target).await?;
            Ok(ToolExecution::new(
                true,
                json!({ "closed": target, "previous_status": previous }).to_string(),
                false,
            ))
        }
        RoutedTool::ResumeAgent => {
            let target = parse_agent_id(&required_any_string_argument(
                &arguments,
                &["id", "agent_id", "target"],
            )?)?;
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
            let header = required_string_argument(&arguments, "header")?;
            let questions = user_input_questions_from_arguments(&arguments)?;
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
        RoutedTool::GithubApiGet => {
            let path = required_string_argument(&arguments, "path")?;
            context
                .ops
                .execute_project_github_api_get(agent, path)
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

async fn upload_file(
    context: &ContainerToolContext<'_>,
    agent_id: AgentId,
    path: &str,
    content_base64: &str,
) -> Result<usize> {
    let bytes = BASE64
        .decode(content_base64.trim())
        .map_err(|err| RuntimeError::InvalidInput(format!("invalid base64: {err}")))?;
    let temp = tempfile::NamedTempFile::new()?;
    std::fs::write(temp.path(), &bytes)?;
    let container_id = context.ops.container_id(agent_id).await?;
    context
        .docker
        .copy_to_container(&container_id, temp.path(), path)
        .await?;
    Ok(bytes.len())
}

async fn download_file_tar(
    context: &ContainerToolContext<'_>,
    agent_id: AgentId,
    path: &str,
) -> Result<Vec<u8>> {
    let container_id = context.ops.container_id(agent_id).await?;
    Ok(context
        .docker
        .copy_from_container_tar(&container_id, path)
        .await?)
}

pub(crate) async fn run_tool_call<F, Fut>(
    store: &ConfigStore,
    events: &RuntimeEvents,
    agent: &Arc<AgentRecord>,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    call_id: &str,
    name: &str,
    arguments: Value,
    execute: F,
) -> Result<ToolExecution>
where
    F: FnOnce(Value) -> Fut,
    Fut: Future<Output = Result<ToolExecution>>,
{
    let arguments_preview = trace_preview_value(&arguments, 500);
    let inline_arguments = inline_event_arguments(&arguments);
    let trace_arguments = arguments.clone();
    let started_wall_time = now();
    super::persistence::record_agent_log(
        store,
        agent_id,
        Some(session_id),
        Some(turn_id),
        "info",
        "tool",
        "tool started",
        json!({
            "call_id": call_id,
            "tool_name": name,
            "arguments_preview": arguments_preview.as_str(),
        }),
    )
    .await;
    super::persistence::record_tool_trace_started(
        store,
        ToolTraceDetail {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            call_id: call_id.to_string(),
            tool_name: name.to_string(),
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
    events
        .publish(ServiceEventKind::ToolStarted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
            call_id: call_id.to_string(),
            tool_name: name.to_string(),
            arguments_preview: Some(arguments_preview),
            arguments: inline_arguments,
        })
        .await;

    let started_at = Instant::now();
    let output = execute(arguments).await;
    let duration_ms = u128_to_u64(started_at.elapsed().as_millis());
    let execution = match output {
        Ok(execution) => execution,
        Err(RuntimeError::TurnCancelled) => return Err(RuntimeError::TurnCancelled),
        Err(err) => ToolExecution::new(false, err.to_string(), false),
    };

    super::history::record_history_item(
        store,
        agent,
        agent_id,
        session_id,
        ModelInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: execution.model_output.clone(),
        },
    )
    .await?;

    let completed_wall_time = now();
    let output_preview = trace_preview_output(&execution.output, 500);
    super::persistence::record_tool_trace_completed(
        store,
        ToolTraceDetail {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            call_id: call_id.to_string(),
            tool_name: name.to_string(),
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
        store,
        agent_id,
        Some(session_id),
        Some(turn_id),
        if execution.success { "info" } else { "warn" },
        "tool",
        "tool completed",
        json!({
            "call_id": call_id,
            "tool_name": name,
            "success": execution.success,
            "duration_ms": duration_ms,
            "output_preview": output_preview.as_str(),
        }),
    )
    .await;
    events
        .publish(ServiceEventKind::ToolCompleted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
            call_id: call_id.to_string(),
            tool_name: name.to_string(),
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

fn required_any_string_argument(arguments: &Value, fields: &[&str]) -> Result<String> {
    for field in fields {
        if let Some(value) = arguments
            .get(field)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        {
            return Ok(value);
        }
    }
    Err(RuntimeError::InvalidInput(format!(
        "missing string field `{}`",
        fields.join("` or `")
    )))
}

fn optional_string_argument(arguments: &Value, field: &str) -> Option<String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn optional_usize_argument(arguments: &Value, field: &str) -> Result<Option<usize>> {
    let Some(value) = arguments.get(field) else {
        return Ok(None);
    };
    let raw = value
        .as_u64()
        .ok_or_else(|| RuntimeError::InvalidInput(format!("field `{field}` must be an integer")))?;
    usize::try_from(raw)
        .map(Some)
        .map_err(|_| RuntimeError::InvalidInput(format!("field `{field}` is too large")))
}

fn parse_agent_id(value: &str) -> Result<AgentId> {
    Uuid::parse_str(value)
        .map_err(|err| RuntimeError::InvalidInput(format!("invalid agent_id `{value}`: {err}")))
}

fn parse_session_id(value: &str) -> Result<SessionId> {
    Uuid::parse_str(value)
        .map_err(|err| RuntimeError::InvalidInput(format!("invalid session_id `{value}`: {err}")))
}

fn parse_agent_role(value: &str) -> Result<AgentRole> {
    match value.trim().to_lowercase().as_str() {
        "" | "executor" => Ok(AgentRole::Executor),
        "planner" => Ok(AgentRole::Planner),
        "explorer" => Ok(AgentRole::Explorer),
        "reviewer" => Ok(AgentRole::Reviewer),
        _ => Err(RuntimeError::InvalidInput(format!(
            "invalid agent role `{value}`; expected planner, explorer, executor, or reviewer"
        ))),
    }
}

fn spawn_agent_tool_request(arguments: &Value) -> Result<SpawnAgentToolRequest> {
    let legacy_role = optional_string_argument(arguments, "role")
        .as_deref()
        .map(parse_agent_role)
        .transpose()?;
    let role = legacy_role
        .or_else(|| {
            optional_string_argument(arguments, "agent_type")
                .and_then(|value| agents::agent_type_role(&value))
        })
        .unwrap_or_default();
    Ok(SpawnAgentToolRequest {
        name: optional_string_argument(arguments, "name"),
        role,
        legacy_role,
        model: optional_string_argument(arguments, "model"),
        reasoning_effort: optional_string_argument(arguments, "reasoning_effort"),
        fork_context: arguments
            .get("fork_context")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        collab_input: collab_input_from_args(arguments)?,
    })
}

fn todo_items_from_arguments(arguments: &Value) -> Result<Vec<TodoItem>> {
    let Some(items_arg) = arguments.get("items").or_else(|| arguments.get("todos")) else {
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
    serde_json::from_value(items_value)
        .map_err(|e| RuntimeError::InvalidInput(format!("invalid items: {e}")))
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
        arguments, "agent_id",
    )?)?])
}

fn wait_timeout(arguments: &Value) -> std::time::Duration {
    if let Some(ms) = arguments.get("timeout_ms").and_then(Value::as_u64) {
        return std::time::Duration::from_millis(ms);
    }
    std::time::Duration::from_secs(
        arguments
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_WAIT_AGENT_OBSERVATION_SECS),
    )
}

fn user_input_questions_from_arguments(arguments: &Value) -> Result<Vec<UserInputQuestion>> {
    let questions_arg = arguments
        .get("questions")
        .ok_or_else(|| RuntimeError::InvalidInput("missing field `questions`".to_string()))?;
    let raw_questions: Vec<Value> = serde_json::from_value(questions_arg.clone())
        .map_err(|e| RuntimeError::InvalidInput(format!("invalid questions: {e}")))?;
    let mut questions = Vec::with_capacity(raw_questions.len());
    for raw in &raw_questions {
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
    Ok(questions)
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
        "bytes_returned": text.len(),
        "bytes_omitted": bytes_omitted,
        "next_offset": next_offset,
        "text": text,
    })
    .to_string()
}

fn bounded_json_tool_output(mut value: Value, max_bytes: usize) -> Value {
    match &mut value {
        Value::Object(map) => {
            for key in ["stdout", "stderr", "body", "text", "tar_base64"] {
                if let Some(Value::String(text)) = map.get_mut(key) {
                    let (bounded, truncated, bytes_omitted, next_offset) =
                        bounded_text(text, max_bytes, 0);
                    if truncated {
                        *text = bounded;
                        map.insert("truncated".to_string(), Value::Bool(true));
                        map.insert("bytes_returned".to_string(), json!(max_bytes));
                        map.insert("bytes_omitted".to_string(), json!(bytes_omitted));
                        map.insert("next_offset".to_string(), json!(next_offset));
                        break;
                    }
                }
            }
            if value.to_string().len() > max_bytes {
                let serialized = value.to_string();
                let (text, _, bytes_omitted, next_offset) = bounded_text(&serialized, max_bytes, 0);
                json!({
                    "truncated": true,
                    "bytes_returned": text.len(),
                    "bytes_omitted": bytes_omitted,
                    "next_offset": next_offset,
                    "json_preview": text,
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
                    "bytes_returned": text.len(),
                    "bytes_omitted": bytes_omitted,
                    "next_offset": next_offset,
                    "json_preview": text,
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

fn u128_to_u64(value: u128) -> u64 {
    value.min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{AgentSessionSummary, AgentStatus, AgentSummary, ModelInputItem, now};
    use mai_store::ToolTraceFilter;
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

        async fn history(&self) -> Vec<ModelInputItem> {
            self.agent.sessions.lock().await[0].history.clone()
        }
    }

    #[tokio::test]
    async fn tool_lifecycle_records_trace_event_and_function_call_output() {
        let harness = Harness::new().await;
        let execution = run_tool_call(
            harness.store.as_ref(),
            &harness.events,
            &harness.agent,
            harness.agent_id,
            harness.session_id,
            harness.turn_id,
            "call_1",
            "demo_tool",
            json!({ "token": "secret", "path": "src/lib.rs" }),
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
        assert!(matches!(
            &history[0],
            ModelInputItem::FunctionCallOutput { call_id, output }
                if call_id == "call_1" && output.contains("\"ok\":true")
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
            harness.store.as_ref(),
            &harness.events,
            &harness.agent,
            harness.agent_id,
            harness.session_id,
            harness.turn_id,
            "call_1",
            "demo_tool",
            json!({}),
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
            .or_else(|| model_value.get("json_preview"))
            .and_then(Value::as_str)
            .expect("visible output");
        assert!(visible.len() <= DEFAULT_MODEL_TOOL_OUTPUT_BYTES);
    }
}
