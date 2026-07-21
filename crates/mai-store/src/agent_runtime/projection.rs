use crate::records::{SessionEventJournalRecord, SessionViewSnapshotRecord};
use crate::{Db, List, MaiStore, Query, Result, i64_to_u64};

use super::{StoredAgentRuntimeSession, StoredSessionEvent, StoredSessionProjection};

impl MaiStore {
    /// 按 framework session ID 加载 canonical projection 与 durable journal。
    pub async fn load_session_projection(
        &self,
        session_id: &str,
    ) -> Result<Option<StoredSessionProjection>> {
        let mut db = self.db.clone();
        load_session_projection(&mut db, session_id).await
    }
}

pub(super) async fn load_session_projections(
    db: &mut Db,
    sessions: &[StoredAgentRuntimeSession],
) -> Result<Vec<StoredSessionProjection>> {
    let mut projections = Vec::new();
    for session in sessions {
        if let Some(projection) = load_session_projection(db, &session.session_id).await? {
            projections.push(projection);
        }
    }
    Ok(projections)
}

async fn load_session_projection(
    db: &mut Db,
    session_id: &str,
) -> Result<Option<StoredSessionProjection>> {
    let mut snapshots = Query::<List<SessionViewSnapshotRecord>>::filter(
        SessionViewSnapshotRecord::fields()
            .session_id()
            .eq(session_id.to_string()),
    )
    .exec(&mut *db)
    .await?;
    let Some(snapshot) = snapshots.pop() else {
        return Ok(None);
    };
    let mut events = Query::<List<SessionEventJournalRecord>>::filter(
        SessionEventJournalRecord::fields()
            .session_id()
            .eq(session_id.to_string()),
    )
    .exec(&mut *db)
    .await?;
    events.sort_by_key(|event| event.sequence);
    Ok(Some(StoredSessionProjection {
        session_id: session_id.to_string(),
        through_sequence: i64_to_u64(snapshot.through_sequence),
        snapshot: serde_json::from_str(&snapshot.snapshot_json)?,
        durable_events: events
            .into_iter()
            .map(|event| {
                Ok(StoredSessionEvent {
                    sequence: i64_to_u64(event.sequence),
                    emitted_at: event.emitted_at,
                    payload: serde_json::from_str(&event.event_json)?,
                })
            })
            .collect::<Result<_>>()?,
    }))
}
