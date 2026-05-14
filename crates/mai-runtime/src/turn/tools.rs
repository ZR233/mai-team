use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mai_mcp::McpTool;
use mai_protocol::{
    AgentId, ModelInputItem, ServiceEventKind, SessionId, ToolOutputArtifactInfo, ToolTraceDetail,
    TurnId, now, preview,
};
use mai_store::ConfigStore;
use mai_tools::{RoutedTool, route_tool};
use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use crate::events::RuntimeEvents;
use crate::state::{AgentRecord, RuntimeState};
use crate::{Result, RuntimeError};

const TOKEN_ESTIMATE_BYTES: usize = 4;
pub(crate) const DEFAULT_MODEL_TOOL_OUTPUT_TOKENS: usize = 10_000;
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

fn parse_agent_id(value: &str) -> Result<AgentId> {
    Uuid::parse_str(value)
        .map_err(|err| RuntimeError::InvalidInput(format!("invalid agent_id `{value}`: {err}")))
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
