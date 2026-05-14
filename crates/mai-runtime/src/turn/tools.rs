use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mai_protocol::{
    AgentId, ModelInputItem, ServiceEventKind, SessionId, ToolOutputArtifactInfo, ToolTraceDetail,
    TurnId, now, preview,
};
use mai_store::ConfigStore;
use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use crate::events::RuntimeEvents;
use crate::state::{AgentRecord, ToolExecution, ToolOutputCapture};
use crate::{Result, RuntimeError};

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
            store
                .save_agent(&summary, None)
                .await
                .expect("save agent");
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
        assert!(!harness.events.snapshot().await.iter().any(|event| matches!(
            event.kind,
            ServiceEventKind::ToolCompleted { .. }
        )));
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
}
