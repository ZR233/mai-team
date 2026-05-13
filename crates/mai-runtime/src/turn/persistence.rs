use chrono::{DateTime, Utc};
use mai_protocol::{AgentId, AgentLogEntry, SessionId, ToolTraceDetail, TurnId, now};
use mai_store::ConfigStore;
use serde_json::Value;
use uuid::Uuid;

pub(crate) async fn record_agent_log(
    store: &ConfigStore,
    agent_id: AgentId,
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
    level: &str,
    category: &str,
    message: &str,
    details: Value,
) {
    let entry = AgentLogEntry {
        id: Uuid::new_v4(),
        agent_id,
        session_id,
        turn_id,
        level: level.to_string(),
        category: category.to_string(),
        message: message.to_string(),
        details,
        timestamp: now(),
    };
    if let Err(err) = store.append_agent_log_entry(&entry).await {
        tracing::warn!(agent_id = %agent_id, "failed to persist agent log entry: {err}");
    }
}

pub(crate) async fn record_tool_trace_started(
    store: &ConfigStore,
    trace: ToolTraceDetail,
    started_at: DateTime<Utc>,
) {
    if let Err(err) = store.save_tool_trace_started(&trace, started_at).await {
        tracing::warn!(agent_id = %trace.agent_id, call_id = %trace.call_id, "failed to persist tool trace start: {err}");
    }
}

pub(crate) async fn record_tool_trace_completed(
    store: &ConfigStore,
    trace: ToolTraceDetail,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) {
    if let Err(err) = store
        .save_tool_trace_completed(&trace, started_at, completed_at)
        .await
    {
        tracing::warn!(agent_id = %trace.agent_id, call_id = %trace.call_id, "failed to persist tool trace completion: {err}");
    }
}
