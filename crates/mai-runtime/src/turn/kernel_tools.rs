use chrono::{DateTime, Utc};
use mai_protocol::{
    AgentId, ServiceEventKind, SessionId, ToolOutputArtifactInfo, ToolTraceDetail, TurnId, now,
};
use pl_core::{ToolLifecyclePhase, ToolLifecycleProjection};
use pl_trace::TraceEvent;
use serde_json::json;

use crate::AgentRuntime;
use crate::turn::persistence::AgentLogRecord;

/// 将 pl-core trace 投影为 mai-store 和 Web UI 仍在消费的 tool lifecycle 事件。
pub(crate) async fn project_tool_trace_events(
    runtime: &AgentRuntime,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    events: &[TraceEvent],
) {
    for projection in pl_core::tool_lifecycle_projections(events, 500) {
        match &projection.phase {
            ToolLifecyclePhase::Started => {
                project_tool_started(runtime, agent_id, session_id, turn_id, &projection).await;
            }
            ToolLifecyclePhase::Finished { success } => {
                project_tool_completed(
                    runtime,
                    agent_id,
                    session_id,
                    turn_id,
                    &projection,
                    *success,
                )
                .await;
            }
        }
    }
}

async fn project_tool_started(
    runtime: &AgentRuntime,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    projection: &ToolLifecycleProjection,
) {
    let started_at = trace_time(projection.started_at_unix);
    super::persistence::record_tool_trace_started(
        runtime.deps.store.as_ref(),
        ToolTraceDetail {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            call_id: projection.call_id.clone(),
            tool_name: projection.tool_name.clone(),
            arguments: projection.arguments.clone(),
            output: String::new(),
            success: false,
            duration_ms: None,
            started_at: Some(started_at),
            completed_at: None,
            output_preview: String::new(),
            output_artifacts: Vec::new(),
        },
        started_at,
    )
    .await;
    super::persistence::record_agent_log(
        runtime.deps.store.as_ref(),
        AgentLogRecord {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            level: "info",
            category: "tool",
            message: "tool started",
            details: json!({
                "call_id": projection.call_id,
                "tool_name": projection.tool_name,
                "arguments_preview": projection.arguments_preview,
            }),
        },
    )
    .await;
    runtime
        .events
        .publish(ServiceEventKind::ToolStarted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
            call_id: projection.call_id.clone(),
            tool_name: projection.tool_name.clone(),
            arguments_preview: Some(projection.arguments_preview.clone()),
            arguments: Some(projection.arguments.clone()),
        })
        .await;
}

async fn project_tool_completed(
    runtime: &AgentRuntime,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    projection: &ToolLifecycleProjection,
    success: bool,
) {
    let started_at = trace_time(projection.started_at_unix);
    let completed_at = trace_time(projection.completed_at_unix_or_started());
    let output_artifacts = projection.output_artifacts_as::<ToolOutputArtifactInfo>();
    super::persistence::record_tool_trace_completed(
        runtime.deps.store.as_ref(),
        ToolTraceDetail {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            call_id: projection.call_id.clone(),
            tool_name: projection.tool_name.clone(),
            arguments: projection.arguments.clone(),
            output: projection.output.clone(),
            success,
            duration_ms: projection.duration_ms,
            started_at: Some(started_at),
            completed_at: Some(completed_at),
            output_preview: projection.output_preview.clone(),
            output_artifacts,
        },
        started_at,
        completed_at,
    )
    .await;
    super::persistence::record_agent_log(
        runtime.deps.store.as_ref(),
        AgentLogRecord {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            level: if success { "info" } else { "warn" },
            category: "tool",
            message: "tool completed",
            details: json!({
                "call_id": projection.call_id,
                "tool_name": projection.tool_name,
                "success": success,
                "duration_ms": projection.duration_ms,
                "output_preview": projection.output_preview.as_str(),
            }),
        },
    )
    .await;
    runtime
        .events
        .publish(ServiceEventKind::ToolCompleted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
            call_id: projection.call_id.clone(),
            tool_name: projection.tool_name.clone(),
            success,
            output_preview: projection.output_preview.clone(),
            duration_ms: projection.duration_ms,
        })
        .await;
}

fn trace_time(seconds: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(seconds, 0).unwrap_or_else(now)
}
