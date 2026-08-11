use mai_protocol::{
    ThreadContextDisposition, ThreadNotificationEnvelope, ThreadSnapshot, ThreadTurnHistory,
    ThreadTurnPage, Turn, TurnBillingRecord,
};

use crate::records::{
    ThreadItemRecord, ThreadNotificationRecord, ThreadRuntimeDocumentRecord,
    ThreadRuntimeEventRecord, ThreadRuntimeTraceRecord, ThreadTurnRecord,
};
use crate::*;

/// 不依赖 pl-core 的 ThreadActor durable document。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredThreadRuntime {
    pub thread_id: String,
    pub revision: u64,
    pub document: serde_json::Value,
    pub snapshot: Option<ThreadSnapshot>,
    pub updated_at: i64,
}

/// 随 Thread commit 原子保存的 framework runtime event。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredThreadRuntimeEvent {
    pub sequence: u64,
    pub created_at: i64,
    pub payload: serde_json::Value,
}

/// 随 Thread commit 原子保存的 trace event。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredThreadTraceEvent {
    pub sequence: u64,
    pub payload: serde_json::Value,
}

/// mai-runtime 交给 store 的完整 Thread CAS commit。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadRuntimeCommitDocument {
    pub expected_revision: Option<u64>,
    pub runtime: StoredThreadRuntime,
    pub turn: Option<ThreadRuntimeTurnCommit>,
    pub notifications: Vec<ThreadNotificationEnvelope>,
    pub runtime_events: Vec<StoredThreadRuntimeEvent>,
    pub trace_events: Vec<StoredThreadTraceEvent>,
}

/// 同一 Thread transaction 中对 Turn 状态与 inference billing 的原子更新。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadRuntimeTurnCommit {
    pub id: String,
    pub thread_id: String,
    pub turn: Option<Turn>,
    pub billing: Option<TurnBillingRecord>,
}

/// Thread CAS commit 的稳定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadRuntimeCommitOutcome {
    Applied,
    RevisionConflict { actual_revision: Option<u64> },
}

impl MaiStore {
    /// 加载全部 canonical ThreadActor 文档。
    pub async fn load_thread_runtimes(&self) -> Result<Vec<StoredThreadRuntime>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<ThreadRuntimeDocumentRecord>>::all()
            .exec(&mut db)
            .await?;
        rows.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
        rows.into_iter().map(stored_runtime).collect()
    }

    /// 加载单个 canonical ThreadActor 文档。
    pub async fn load_thread_runtime(
        &self,
        thread_id: &str,
    ) -> Result<Option<StoredThreadRuntime>> {
        let mut db = self.db.clone();
        Query::<List<ThreadRuntimeDocumentRecord>>::filter(
            ThreadRuntimeDocumentRecord::fields()
                .thread_id()
                .eq(thread_id.to_string()),
        )
        .exec(&mut db)
        .await?
        .into_iter()
        .next()
        .map(stored_runtime)
        .transpose()
    }

    /// 原子校验 runtime revision 并保存 Thread、Turn、Item 与通知。
    pub async fn commit_thread_runtime(
        &self,
        document: ThreadRuntimeCommitDocument,
    ) -> Result<ThreadRuntimeCommitOutcome> {
        crate::sqlite_busy::retry_sqlite_busy(|| async {
            self.commit_thread_runtime_once(&document).await
        })
        .await
    }

    async fn commit_thread_runtime_once(
        &self,
        document: &ThreadRuntimeCommitDocument,
    ) -> Result<ThreadRuntimeCommitOutcome> {
        let thread_id = document.runtime.thread_id.clone();
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        let existing = Query::<List<ThreadRuntimeDocumentRecord>>::filter(
            ThreadRuntimeDocumentRecord::fields()
                .thread_id()
                .eq(thread_id.clone()),
        )
        .exec(&mut tx)
        .await?;
        let actual_revision = existing.first().map(|record| i64_to_u64(record.revision));
        if actual_revision != document.expected_revision {
            return Ok(ThreadRuntimeCommitOutcome::RevisionConflict { actual_revision });
        }

        let mut runtime = document.runtime.clone();
        if runtime.snapshot.is_none() {
            runtime.snapshot = existing
                .first()
                .and_then(|record| record.snapshot_json.as_deref())
                .map(serde_json::from_str)
                .transpose()?;
        }
        replace_runtime_document(&mut tx, &runtime).await?;
        // projection snapshot 与 actor document 同属本次 CAS；其中同时包含 Item、
        // Interaction 和 runtime overlay。没有 projection 变化时沿用上次 snapshot。
        if let Some(snapshot) = &document.runtime.snapshot {
            replace_thread_items(&mut tx, snapshot).await?;
        }
        if let Some(turn) = &document.turn {
            upsert_thread_turn(&mut tx, turn).await?;
        }
        append_notifications(&mut tx, &document.notifications).await?;
        append_runtime_events(&mut tx, &thread_id, &document.runtime_events).await?;
        append_trace_events(&mut tx, &thread_id, &document.trace_events).await?;
        tx.commit().await?;
        Ok(ThreadRuntimeCommitOutcome::Applied)
    }

    /// 按最新优先分页读取一个 Thread 的 durable Turn 历史。
    pub async fn list_thread_turns(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ThreadTurnPage> {
        let limit = limit.clamp(1, 200);
        let before_ordinal = cursor.map(parse_cursor).transpose()?;
        let mut db = self.db.clone();
        let mut rows = Query::<List<ThreadTurnRecord>>::filter(
            ThreadTurnRecord::fields()
                .thread_id()
                .eq(thread_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.sort_by_key(|row| std::cmp::Reverse(row.ordinal));
        if let Some(before) = before_ordinal {
            rows.retain(|row| row.ordinal < before);
        }
        let has_more = rows.len() > limit;
        rows.truncate(limit);

        let mut turns = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut items = Query::<List<ThreadItemRecord>>::filter(
                ThreadItemRecord::fields().turn_id().eq(row.id.clone()),
            )
            .exec(&mut db)
            .await?;
            items.retain(|item| item.thread_id == thread_id);
            items.sort_by_key(|item| item.ordinal);
            turns.push(ThreadTurnHistory {
                turn: serde_json::from_str(&row.turn_json)?,
                items: items
                    .into_iter()
                    .map(|item| serde_json::from_str(&item.item_json))
                    .collect::<std::result::Result<_, _>>()?,
                context_disposition: serde_json::from_str(&row.context_disposition)?,
            });
        }
        let next_cursor = has_more
            .then(|| rows.last().map(|row| format!("v1:{:x}", row.ordinal)))
            .flatten();
        Ok(ThreadTurnPage { turns, next_cursor })
    }
}

fn stored_runtime(row: ThreadRuntimeDocumentRecord) -> Result<StoredThreadRuntime> {
    Ok(StoredThreadRuntime {
        thread_id: row.thread_id,
        revision: i64_to_u64(row.revision),
        document: serde_json::from_str(&row.document_json)?,
        snapshot: row
            .snapshot_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?,
        updated_at: row.updated_at,
    })
}

async fn replace_runtime_document(
    tx: &mut toasty::Transaction<'_>,
    runtime: &StoredThreadRuntime,
) -> Result<()> {
    Query::<List<ThreadRuntimeDocumentRecord>>::filter(
        ThreadRuntimeDocumentRecord::fields()
            .thread_id()
            .eq(runtime.thread_id.clone()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    toasty::create!(ThreadRuntimeDocumentRecord {
        thread_id: runtime.thread_id.clone(),
        revision: u64_to_i64(runtime.revision),
        document_json: serde_json::to_string(&runtime.document)?,
        snapshot_json: runtime
            .snapshot
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?,
        updated_at: runtime.updated_at,
    })
    .exec(&mut *tx)
    .await?;
    Ok(())
}

async fn replace_thread_items(
    tx: &mut toasty::Transaction<'_>,
    snapshot: &ThreadSnapshot,
) -> Result<()> {
    Query::<List<ThreadItemRecord>>::filter(
        ThreadItemRecord::fields()
            .thread_id()
            .eq(snapshot.thread.id.clone()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    for item in &snapshot.items {
        toasty::create!(ThreadItemRecord {
            id: item.id.clone(),
            thread_id: item.thread_id.clone(),
            turn_id: item.turn_id.clone(),
            ordinal: u64_to_i64(item.ordinal),
            revision: u64_to_i64(item.revision),
            item_json: serde_json::to_string(item)?,
        })
        .exec(&mut *tx)
        .await?;
    }
    Ok(())
}

async fn upsert_thread_turn(
    tx: &mut toasty::Transaction<'_>,
    update: &ThreadRuntimeTurnCommit,
) -> Result<()> {
    let existing = Query::<List<ThreadTurnRecord>>::filter(
        ThreadTurnRecord::fields().id().eq(update.id.clone()),
    )
    .exec(&mut *tx)
    .await?;
    let previous = existing.first();
    if previous.is_some_and(|record| record.thread_id != update.thread_id) {
        return Err(StoreError::InvalidConfig(format!(
            "Turn {} cannot move from another Thread to {}",
            update.id, update.thread_id
        )));
    }
    if let Some(turn) = &update.turn
        && (turn.id != update.id || turn.thread_id != update.thread_id)
    {
        return Err(StoreError::InvalidConfig(format!(
            "Turn commit {} has inconsistent Thread/Turn ownership",
            update.id
        )));
    }
    let turn_json = update
        .turn
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?
        .or_else(|| previous.map(|record| record.turn_json.clone()))
        .ok_or_else(|| {
            StoreError::InvalidConfig(format!(
                "Turn billing commit {} has no durable Turn",
                update.id
            ))
        })?;
    let model_json = update
        .billing
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?
        .or_else(|| previous.and_then(|record| record.model_json.clone()));
    let ordinal = existing
        .first()
        .map(|record| record.ordinal)
        .or_else(|| {
            update
                .turn
                .as_ref()
                .map(|turn| turn.started_at.unwrap_or(turn.updated_at))
        })
        .ok_or_else(|| StoreError::InvalidConfig(format!("Turn {} lacks ordinal", update.id)))?;
    if !existing.is_empty() {
        Query::<List<ThreadTurnRecord>>::filter(
            ThreadTurnRecord::fields().id().eq(update.id.clone()),
        )
        .delete()
        .exec(&mut *tx)
        .await?;
    }
    toasty::create!(ThreadTurnRecord {
        id: update.id.clone(),
        thread_id: update.thread_id.clone(),
        ordinal,
        turn_json,
        model_json,
        context_disposition: previous
            .map(|record| record.context_disposition.clone())
            .unwrap_or(serde_json::to_string(&ThreadContextDisposition::Active)?),
    })
    .exec(&mut *tx)
    .await?;
    Ok(())
}

async fn append_notifications(
    tx: &mut toasty::Transaction<'_>,
    notifications: &[ThreadNotificationEnvelope],
) -> Result<()> {
    for notification in notifications {
        let id = format!("{}:{}", notification.thread_id, notification.revision);
        Query::<List<ThreadNotificationRecord>>::filter(
            ThreadNotificationRecord::fields().id().eq(id.clone()),
        )
        .delete()
        .exec(&mut *tx)
        .await?;
        toasty::create!(ThreadNotificationRecord {
            id,
            thread_id: notification.thread_id.clone(),
            revision: u64_to_i64(notification.revision),
            emitted_at: notification.emitted_at,
            notification_json: serde_json::to_string(notification)?,
        })
        .exec(&mut *tx)
        .await?;
    }
    Ok(())
}

async fn append_runtime_events(
    tx: &mut toasty::Transaction<'_>,
    thread_id: &str,
    events: &[StoredThreadRuntimeEvent],
) -> Result<()> {
    for event in events {
        let id = format!("{thread_id}:{}", event.sequence);
        Query::<List<ThreadRuntimeEventRecord>>::filter(
            ThreadRuntimeEventRecord::fields().id().eq(id.clone()),
        )
        .delete()
        .exec(&mut *tx)
        .await?;
        toasty::create!(ThreadRuntimeEventRecord {
            id,
            thread_id: thread_id.to_string(),
            sequence: u64_to_i64(event.sequence),
            created_at: event.created_at,
            event_json: serde_json::to_string(&event.payload)?,
        })
        .exec(&mut *tx)
        .await?;
    }
    Ok(())
}

async fn append_trace_events(
    tx: &mut toasty::Transaction<'_>,
    thread_id: &str,
    events: &[StoredThreadTraceEvent],
) -> Result<()> {
    for event in events {
        let id = format!("{thread_id}:{}", event.sequence);
        Query::<List<ThreadRuntimeTraceRecord>>::filter(
            ThreadRuntimeTraceRecord::fields().id().eq(id.clone()),
        )
        .delete()
        .exec(&mut *tx)
        .await?;
        toasty::create!(ThreadRuntimeTraceRecord {
            id,
            thread_id: thread_id.to_string(),
            sequence: u64_to_i64(event.sequence),
            trace_json: serde_json::to_string(&event.payload)?,
        })
        .exec(&mut *tx)
        .await?;
    }
    Ok(())
}

fn parse_cursor(cursor: &str) -> Result<i64> {
    let value = cursor
        .strip_prefix("v1:")
        .ok_or_else(|| StoreError::InvalidConfig("invalid Thread cursor".to_string()))?;
    i64::from_str_radix(value, 16)
        .map_err(|error| StoreError::InvalidConfig(format!("invalid Thread cursor: {error}")))
}

#[cfg(test)]
mod tests {
    use mai_protocol::{
        ThreadAttachment, ThreadItem, ThreadItemContent, ThreadItemStatus, ThreadSnapshot, Turn,
        TurnState,
    };
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn revision_conflict_commits_no_partial_turn_or_document() {
        let (_directory, store) = test_store().await;
        assert_eq!(
            store
                .commit_thread_runtime(commit("thread-a", None, 1, None, None))
                .await
                .expect("initial commit"),
            ThreadRuntimeCommitOutcome::Applied
        );
        let conflicting_turn = turn("turn-conflict", "thread-a", 20);
        assert_eq!(
            store
                .commit_thread_runtime(
                    commit("thread-a", Some(0), 2, Some(conflicting_turn), None,)
                )
                .await
                .expect("conflicting commit"),
            ThreadRuntimeCommitOutcome::RevisionConflict {
                actual_revision: Some(1)
            }
        );
        assert_eq!(
            store
                .load_thread_runtime("thread-a")
                .await
                .expect("load runtime")
                .expect("runtime")
                .revision,
            1
        );
        assert!(
            store
                .list_thread_turns("thread-a", None, 20)
                .await
                .expect("turn page")
                .turns
                .is_empty()
        );
    }

    #[tokio::test]
    async fn inference_billing_is_atomic_with_the_thread_revision() {
        let (_directory, store) = test_store().await;
        let initial_turn = turn("turn-a", "thread-a", 10);
        assert_eq!(
            store
                .commit_thread_runtime(commit("thread-a", None, 1, Some(initial_turn), None,))
                .await
                .expect("initial turn"),
            ThreadRuntimeCommitOutcome::Applied
        );

        let billing = TurnBillingRecord::new();
        let mut billing_commit = commit("thread-a", Some(1), 2, None, None);
        billing_commit.turn = Some(ThreadRuntimeTurnCommit {
            id: "turn-a".to_string(),
            thread_id: "thread-a".to_string(),
            turn: None,
            billing: Some(billing.clone()),
        });
        assert_eq!(
            store
                .commit_thread_runtime(billing_commit)
                .await
                .expect("billing commit"),
            ThreadRuntimeCommitOutcome::Applied
        );

        let mut rejected = billing.clone();
        rejected.version = 99;
        let mut conflict = commit("thread-a", Some(1), 3, None, None);
        conflict.turn = Some(ThreadRuntimeTurnCommit {
            id: "turn-a".to_string(),
            thread_id: "thread-a".to_string(),
            turn: None,
            billing: Some(rejected),
        });
        assert_eq!(
            store
                .commit_thread_runtime(conflict)
                .await
                .expect("conflicting billing commit"),
            ThreadRuntimeCommitOutcome::RevisionConflict {
                actual_revision: Some(2)
            }
        );

        let mut db = store.db.clone();
        let record = Query::<List<ThreadTurnRecord>>::filter(
            ThreadTurnRecord::fields().id().eq("turn-a".to_string()),
        )
        .exec(&mut db)
        .await
        .expect("load durable turn")
        .into_iter()
        .next()
        .expect("durable turn");
        assert_eq!(
            serde_json::from_str::<TurnBillingRecord>(
                record.model_json.as_deref().expect("model billing")
            )
            .expect("typed billing"),
            billing
        );
    }

    #[tokio::test]
    async fn interleaved_thread_commits_keep_turns_and_items_isolated() {
        let (_directory, store) = test_store().await;
        for (thread_id, turn_id, text, timestamp) in [
            ("thread-a", "turn-a", "alpha", 10),
            ("thread-b", "turn-b", "beta", 11),
        ] {
            let turn = turn(turn_id, thread_id, timestamp);
            let mut snapshot = ThreadSnapshot::empty(thread_id);
            snapshot.items.push(ThreadItem {
                id: format!("item-{thread_id}"),
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                ordinal: 0,
                revision: 1,
                status: ThreadItemStatus::Completed,
                created_at: timestamp,
                updated_at: timestamp,
                completed_at: Some(timestamp),
                error: None,
                content: ThreadItemContent::UserMessage {
                    text: text.to_string(),
                    attachments: Vec::<ThreadAttachment>::new(),
                },
                usage: None,
            });
            assert_eq!(
                store
                    .commit_thread_runtime(commit(thread_id, None, 1, Some(turn), Some(snapshot),))
                    .await
                    .expect("commit thread"),
                ThreadRuntimeCommitOutcome::Applied
            );
        }

        let alpha = store
            .list_thread_turns("thread-a", None, 20)
            .await
            .expect("alpha page");
        let beta = store
            .list_thread_turns("thread-b", None, 20)
            .await
            .expect("beta page");
        assert_eq!(alpha.turns.len(), 1);
        assert_eq!(beta.turns.len(), 1);
        assert_eq!(alpha.turns[0].turn.id, "turn-a");
        assert_eq!(beta.turns[0].turn.id, "turn-b");
        assert_eq!(alpha.turns[0].items[0].thread_id, "thread-a");
        assert_eq!(beta.turns[0].items[0].thread_id, "thread-b");
    }

    async fn test_store() -> (tempfile::TempDir, MaiStore) {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = MaiStore::open_with_config_and_artifact_index_path(
            directory.path().join("store.sqlite3"),
            directory.path().join("config.toml"),
            directory.path().join("artifacts"),
        )
        .await
        .expect("open store");
        (directory, store)
    }

    fn commit(
        thread_id: &str,
        expected_revision: Option<u64>,
        revision: u64,
        turn: Option<Turn>,
        snapshot: Option<ThreadSnapshot>,
    ) -> ThreadRuntimeCommitDocument {
        ThreadRuntimeCommitDocument {
            expected_revision,
            runtime: StoredThreadRuntime {
                thread_id: thread_id.to_string(),
                revision,
                document: serde_json::json!({ "revision": revision }),
                snapshot,
                updated_at: revision as i64,
            },
            turn: turn.map(|turn| ThreadRuntimeTurnCommit {
                id: turn.id.clone(),
                thread_id: turn.thread_id.clone(),
                turn: Some(turn),
                billing: None,
            }),
            notifications: Vec::new(),
            runtime_events: Vec::new(),
            trace_events: Vec::new(),
        }
    }

    fn turn(id: &str, thread_id: &str, timestamp: i64) -> Turn {
        Turn {
            id: id.to_string(),
            thread_id: thread_id.to_string(),
            state: TurnState::Completed,
            failure: None,
            started_at: Some(timestamp),
            updated_at: timestamp,
            completed_at: Some(timestamp),
        }
    }
}
