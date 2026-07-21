use chrono::{DateTime, Utc};

use super::StoredAgentRuntimeState;
use crate::records::AgentRuntimeStateRecord;
use crate::{Result, i64_to_u64};

pub(super) fn stored_state(row: AgentRuntimeStateRecord) -> Result<StoredAgentRuntimeState> {
    Ok(StoredAgentRuntimeState {
        agent_id: row.agent_id,
        parent_id: row.parent_id,
        role: row.role,
        depth: i64_to_u64(row.depth).min(u64::from(u32::MAX)) as u32,
        lifecycle: row.lifecycle,
        activity: row.activity,
        active_turn_id: row.active_turn_id,
        active_session_id: row.active_session_id,
        pending_inputs: i64_to_u64(row.pending_inputs) as usize,
        last_turn: row
            .last_turn_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?,
        revision: i64_to_u64(row.revision),
        event_sequence: i64_to_u64(row.event_sequence),
        updated_at: row.updated_at,
    })
}

pub(super) fn unix_timestamp_rfc3339(timestamp: i64) -> String {
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .unwrap_or_else(Utc::now)
        .to_rfc3339()
}

pub(super) fn usize_to_i64(value: usize) -> i64 {
    value.min(i64::MAX as usize) as i64
}
