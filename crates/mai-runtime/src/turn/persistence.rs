use chrono::{DateTime, Utc};
use mai_protocol::{AgentId, AgentLogEntry, SessionId, ToolTraceDetail, TurnId, now};
use mai_store::ConfigStore;
use serde_json::Value;
use uuid::Uuid;

pub(crate) struct AgentLogRecord {
    pub(crate) agent_id: AgentId,
    pub(crate) session_id: Option<SessionId>,
    pub(crate) turn_id: Option<TurnId>,
    pub(crate) level: &'static str,
    pub(crate) category: &'static str,
    pub(crate) message: &'static str,
    pub(crate) details: Value,
}

pub(crate) async fn record_agent_log(store: &ConfigStore, record: AgentLogRecord) {
    let entry = AgentLogEntry {
        id: Uuid::new_v4(),
        agent_id: record.agent_id,
        session_id: record.session_id,
        turn_id: record.turn_id,
        level: record.level.to_string(),
        category: record.category.to_string(),
        message: record.message.to_string(),
        details: record.details,
        timestamp: now(),
    };
    if let Err(err) = store.append_agent_log_entry(&entry).await {
        tracing::warn!(agent_id = %record.agent_id, "failed to persist agent log entry: {err}");
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
