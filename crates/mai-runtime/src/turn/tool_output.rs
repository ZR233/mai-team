#[cfg(test)]
use std::sync::Arc;

use mai_protocol::ToolOutputArtifactInfo;
#[cfg(test)]
use mai_protocol::ToolTraceDetail;
#[cfg(test)]
use mai_protocol::TurnId;
#[cfg(test)]
use mai_protocol::now;
#[cfg(test)]
use mai_protocol::{AgentId, ServiceEventKind, SessionId};
#[cfg(test)]
use mai_store::ConfigStore;
#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use tokio::time::Instant;
#[cfg(test)]
use uuid::Uuid;

#[cfg(test)]
use crate::Result;
#[cfg(test)]
use crate::RuntimeError;
#[cfg(test)]
use crate::events::RuntimeEvents;
#[cfg(test)]
use crate::state::AgentRecord;
#[cfg(test)]
use crate::turn::persistence::AgentLogRecord;

#[cfg(test)]
pub(crate) const DEFAULT_MODEL_TOOL_OUTPUT_TOKENS: usize =
    pl_core::DEFAULT_MODEL_TOOL_OUTPUT_TOKENS;
#[cfg(test)]
const DEFAULT_MODEL_TOOL_OUTPUT_BYTES: usize = DEFAULT_MODEL_TOOL_OUTPUT_TOKENS * 4;

pub(crate) type ToolExecution = pl_core::ToolExecutionResult<ToolOutputArtifactInfo>;

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

#[cfg(test)]
pub(crate) fn trace_preview_value(value: &Value, max: usize) -> String {
    pl_core::trace_preview_value(value, max)
}

#[cfg(test)]
pub(crate) fn trace_preview_output(output: &str, max: usize) -> String {
    pl_core::trace_preview_output(output, max)
}

#[cfg(test)]
fn inline_event_arguments(value: &Value) -> Option<Value> {
    let redacted = pl_core::redacted_trace_preview_value(value);
    let serialized = serde_json::to_string(&redacted).ok()?;
    (serialized.len() <= 2_000).then_some(redacted)
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
