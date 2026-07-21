use chrono::{DateTime, Utc};
use mai_protocol::{
    AgentId, AgentLogEntry, SessionId, ToolOutputArtifactInfo, ToolTraceDetail, TurnId,
};
use pl_core::{ToolLifecyclePhase, ToolLifecycleProjection};
use pl_trace::TraceEvent;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::AgentRuntime;

pub(super) struct AgentLogProjection {
    pub(super) agent_id: AgentId,
    pub(super) session_id: Option<SessionId>,
    pub(super) turn_id: Option<TurnId>,
    pub(super) level: &'static str,
    pub(super) category: &'static str,
    pub(super) message: &'static str,
    pub(super) details: Value,
    pub(super) timestamp: DateTime<Utc>,
}

/// 将同一 framework transaction 已持久化的 trace 投影为产品观测记录。
pub(super) async fn project_trace_events(
    runtime: &AgentRuntime,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    events: &[TraceEvent],
) {
    for projection in pl_core::tool_lifecycle_projections(events, 500) {
        match projection.phase() {
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
    let started_at = trace_time(projection.started_at_unix());
    let trace = ToolTraceDetail {
        agent_id,
        session_id: Some(session_id),
        turn_id: Some(turn_id),
        call_id: projection.call_id().to_string(),
        tool_name: projection.tool_name().to_string(),
        arguments: projection.arguments().clone(),
        output: String::new(),
        success: false,
        duration_ms: None,
        started_at: Some(started_at),
        completed_at: None,
        output_preview: String::new(),
        output_artifacts: Vec::new(),
    };
    if let Err(error) = runtime
        .deps
        .store
        .save_tool_trace_started(&trace, started_at)
        .await
    {
        tracing::warn!(%agent_id, call_id = %trace.call_id, "failed to project tool trace start: {error}");
    }
    record_agent_log(
        runtime,
        AgentLogProjection {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            level: "info",
            category: "tool",
            message: "tool started",
            details: json!({
                "call_id": projection.call_id(),
                "tool_name": projection.tool_name(),
                "arguments_preview": projection.arguments_preview(),
            }),
            timestamp: started_at,
        },
    )
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
    let started_at = trace_time(projection.started_at_unix());
    let completed_at = trace_time(projection.completed_at_unix_or_started());
    let trace = ToolTraceDetail {
        agent_id,
        session_id: Some(session_id),
        turn_id: Some(turn_id),
        call_id: projection.call_id().to_string(),
        tool_name: projection.tool_name().to_string(),
        arguments: projection.arguments().clone(),
        output: projection.output().to_string(),
        success,
        duration_ms: projection.duration_ms(),
        started_at: Some(started_at),
        completed_at: Some(completed_at),
        output_preview: projection.output_preview().to_string(),
        output_artifacts: projection.output_artifacts_as::<ToolOutputArtifactInfo>(),
    };
    if let Err(error) = runtime
        .deps
        .store
        .save_tool_trace_completed(&trace, started_at, completed_at)
        .await
    {
        tracing::warn!(%agent_id, call_id = %trace.call_id, "failed to project completed tool trace: {error}");
    }
    record_agent_log(
        runtime,
        AgentLogProjection {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            level: if success { "info" } else { "warn" },
            category: "tool",
            message: "tool completed",
            details: json!({
                "call_id": projection.call_id(),
                "tool_name": projection.tool_name(),
                "success": success,
                "duration_ms": projection.duration_ms(),
                "output_preview": projection.output_preview(),
                "raw_bytes": projection.output_metrics().map(|metrics| metrics.raw_bytes),
                "model_visible_bytes": projection.output_metrics().map(|metrics| metrics.model_visible_bytes),
                "artifact_bytes": projection.output_metrics().map(|metrics| metrics.artifact_bytes),
                "result_hash": projection.output_metrics().map(|metrics| metrics.result_hash.as_str()),
                "cache_hit": projection.output_metrics().is_some_and(|metrics| metrics.cache_hit),
            }),
            timestamp: completed_at,
        },
    )
    .await;
}

pub(super) async fn record_agent_log(runtime: &AgentRuntime, projection: AgentLogProjection) {
    let AgentLogProjection {
        agent_id,
        session_id,
        turn_id,
        level,
        category,
        message,
        details,
        timestamp,
    } = projection;
    let entry = AgentLogEntry {
        id: Uuid::new_v4(),
        agent_id,
        session_id,
        turn_id,
        level: level.to_string(),
        category: category.to_string(),
        message: message.to_string(),
        details,
        timestamp,
    };
    if let Err(error) = runtime.deps.store.append_agent_log_entry(&entry).await {
        tracing::warn!(%agent_id, "failed to project agent log: {error}");
    }
}

pub(super) fn trace_time(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).unwrap_or_else(Utc::now)
}
