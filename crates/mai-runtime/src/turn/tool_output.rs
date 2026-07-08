use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
use mai_protocol::ToolTraceDetail;
#[cfg(test)]
use mai_protocol::TurnId;
use mai_protocol::{AgentId, ToolOutputArtifactInfo, now};
#[cfg(test)]
use mai_protocol::{ServiceEventKind, SessionId};
#[cfg(test)]
use mai_store::ConfigStore;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use tokio::time::Instant;
use uuid::Uuid;

use crate::Result;
#[cfg(test)]
use crate::RuntimeError;
#[cfg(test)]
use crate::events::RuntimeEvents;
#[cfg(test)]
use crate::state::AgentRecord;
#[cfg(test)]
use crate::turn::persistence::AgentLogRecord;

pub(crate) const DEFAULT_MODEL_TOOL_OUTPUT_TOKENS: usize =
    pl_core::DEFAULT_MODEL_TOOL_OUTPUT_TOKENS;
#[cfg(test)]
const DEFAULT_MODEL_TOOL_OUTPUT_BYTES: usize = DEFAULT_MODEL_TOOL_OUTPUT_TOKENS * 4;

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
        let model_output =
            pl_core::model_visible_tool_output_with_tokens(&output, max_output_tokens);
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
    pl_core::trace_preview_value(value, max)
}

pub(crate) fn trace_preview_output(output: &str, max: usize) -> String {
    pl_core::trace_preview_output(output, max)
}

#[cfg(test)]
fn inline_event_arguments(value: &Value) -> Option<Value> {
    let redacted = pl_core::redacted_trace_preview_value(value);
    let serialized = serde_json::to_string(&redacted).ok()?;
    (serialized.len() <= 2_000).then_some(redacted)
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
