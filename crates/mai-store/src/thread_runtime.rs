use mai_protocol::{
    ThreadNotificationEnvelope, ThreadSnapshot, ThreadTurnHistory, ThreadTurnPage, Turn,
    TurnBillingRecord,
};
use std::sync::Arc;

use crate::records::{
    ThreadItemRecord, ThreadRuntimeDocumentRecord, ThreadSubmissionRecord, ThreadTurnRecord,
};
use crate::*;

mod commit;

/// 不依赖 pl-core 的 ThreadActor durable document。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredThreadRuntime {
    pub thread_id: String,
    pub revision: u64,
    pub document: serde_json::Value,
    pub snapshot: Option<ThreadSnapshot>,
    pub updated_at: i64,
}

/// 随 Thread commit 原子追加的 durable 阶段提交记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredThreadSubmission {
    pub thread_id: String,
    pub ordinal: u64,
    pub created_at: i64,
    pub submission: serde_json::Value,
}

/// `list_thread_submissions` 的分页结果；`submission` 是记录的原始 JSON。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredThreadSubmissionPage {
    pub items: Vec<StoredThreadSubmission>,
    pub offset: usize,
    pub limit: usize,
    pub total: usize,
    pub has_more: bool,
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
    pub submissions: Vec<StoredThreadSubmission>,
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
        let prepared = Arc::new(
            tokio::task::spawn_blocking(move || commit::PreparedThreadCommit::try_new(document))
                .await
                .map_err(|error| {
                    StoreError::InvalidConfig(format!(
                        "Thread commit preparation task failed: {error}"
                    ))
                })??,
        );
        let path = self.path.clone();
        crate::sqlite_busy::retry_sqlite_busy(|| {
            let path = path.clone();
            let prepared = Arc::clone(&prepared);
            async move {
                tokio::task::spawn_blocking(move || prepared.commit_on_path(&path))
                    .await
                    .map_err(|error| {
                        StoreError::InvalidConfig(format!("Thread commit task failed: {error}"))
                    })?
            }
        })
        .await
    }

    /// 按提交顺序分页读取一个 Thread 的 durable 阶段提交历史。
    pub async fn list_thread_submissions(
        &self,
        thread_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<StoredThreadSubmissionPage> {
        let limit = limit.max(1);
        let mut db = self.db.clone();
        let mut rows = Query::<List<ThreadSubmissionRecord>>::filter(
            ThreadSubmissionRecord::fields()
                .thread_id()
                .eq(thread_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.sort_by_key(|row| row.ordinal);
        let total = rows.len();
        let has_more = offset.saturating_add(limit) < total;
        let items = rows
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|row| {
                Ok(StoredThreadSubmission {
                    thread_id: row.thread_id,
                    ordinal: i64_to_u64(row.ordinal),
                    created_at: row.created_at,
                    submission: serde_json::from_str(&row.submission_json)?,
                })
            })
            .collect::<Result<_>>()?;
        Ok(StoredThreadSubmissionPage {
            items,
            offset,
            limit,
            total,
            has_more,
        })
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

fn parse_cursor(cursor: &str) -> Result<i64> {
    let value = cursor
        .strip_prefix("v1:")
        .ok_or_else(|| StoreError::InvalidConfig("invalid Thread cursor".to_string()))?;
    i64::from_str_radix(value, 16)
        .map_err(|error| StoreError::InvalidConfig(format!("invalid Thread cursor: {error}")))
}

#[cfg(test)]
mod tests {
    use mai_protocol::{ThreadAttachment, ThreadItem, ThreadSnapshot, Turn, TurnState};
    use pl_protocol::{CompletedTurnState, TurnCompletion};
    use pretty_assertions::assert_eq;
    use rusqlite::Connection;

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
            snapshot.items.push(ThreadItem::completed_user_message(
                format!("item-{thread_id}"),
                thread_id.to_string(),
                turn_id.to_string(),
                text.to_string(),
                Vec::<ThreadAttachment>::new(),
                timestamp,
            ));
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

    #[tokio::test]
    async fn consecutive_snapshots_only_write_changed_items() {
        let (_directory, store) = test_store().await;
        let connection = Connection::open(store.path()).expect("open audit connection");
        connection
            .execute_batch(
                r#"
                CREATE TABLE thread_item_audit (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    operation TEXT NOT NULL,
                    item_id TEXT NOT NULL
                );
                CREATE TRIGGER audit_thread_item_insert
                AFTER INSERT ON thread_items
                BEGIN
                    INSERT INTO thread_item_audit(operation, item_id)
                    VALUES ('insert', NEW.id);
                END;
                CREATE TRIGGER audit_thread_item_delete
                AFTER DELETE ON thread_items
                BEGIN
                    INSERT INTO thread_item_audit(operation, item_id)
                    VALUES ('delete', OLD.id);
                END;
                CREATE TRIGGER audit_thread_item_update
                AFTER UPDATE ON thread_items
                BEGIN
                    INSERT INTO thread_item_audit(operation, item_id)
                    VALUES ('update', NEW.id);
                END;
                "#,
            )
            .expect("install item audit triggers");

        let items = ["item-a", "item-b"].map(|id| {
            ThreadItem::completed_user_message(
                id.to_string(),
                "thread-a".to_string(),
                "turn-a".to_string(),
                id.to_string(),
                Vec::<ThreadAttachment>::new(),
                10,
            )
        });
        let mut first_snapshot = ThreadSnapshot::empty("thread-a");
        first_snapshot.items.extend(items.clone());
        store
            .commit_thread_runtime(commit("thread-a", None, 1, None, Some(first_snapshot)))
            .await
            .expect("initial snapshot");
        let baseline: i64 = connection
            .query_row(
                "SELECT COALESCE(MAX(sequence), 0) FROM thread_item_audit",
                [],
                |row| row.get(0),
            )
            .expect("audit baseline");

        let mut second_snapshot = ThreadSnapshot::empty("thread-a");
        second_snapshot.items.extend(items);
        second_snapshot.items[1].updated_at = 11;
        store
            .commit_thread_runtime(commit("thread-a", Some(1), 2, None, Some(second_snapshot)))
            .await
            .expect("updated snapshot");

        let mut statement = connection
            .prepare(
                "SELECT operation, item_id FROM thread_item_audit \
                 WHERE sequence > ?1 ORDER BY sequence",
            )
            .expect("prepare audit query");
        let writes = statement
            .query_map([baseline], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query audit")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect audit rows");
        assert!(!writes.is_empty(), "changed item must be persisted");
        assert_eq!(
            writes
                .iter()
                .map(|(_, item_id)| item_id.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from(["item-b"]),
        );
    }

    #[tokio::test]
    async fn replayed_thread_facts_update_without_delete_churn() {
        let (_directory, store) = test_store().await;
        let connection = Connection::open(store.path()).expect("open audit connection");
        connection
            .execute_batch(
                r#"
                CREATE TABLE thread_fact_audit (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    fact TEXT NOT NULL,
                    operation TEXT NOT NULL
                );
                CREATE TRIGGER audit_runtime_event_insert AFTER INSERT ON thread_runtime_events
                BEGIN INSERT INTO thread_fact_audit(fact, operation) VALUES ('event', 'insert'); END;
                CREATE TRIGGER audit_runtime_event_update AFTER UPDATE ON thread_runtime_events
                BEGIN INSERT INTO thread_fact_audit(fact, operation) VALUES ('event', 'update'); END;
                CREATE TRIGGER audit_runtime_event_delete AFTER DELETE ON thread_runtime_events
                BEGIN INSERT INTO thread_fact_audit(fact, operation) VALUES ('event', 'delete'); END;
                CREATE TRIGGER audit_trace_insert AFTER INSERT ON thread_runtime_traces
                BEGIN INSERT INTO thread_fact_audit(fact, operation) VALUES ('trace', 'insert'); END;
                CREATE TRIGGER audit_trace_update AFTER UPDATE ON thread_runtime_traces
                BEGIN INSERT INTO thread_fact_audit(fact, operation) VALUES ('trace', 'update'); END;
                CREATE TRIGGER audit_trace_delete AFTER DELETE ON thread_runtime_traces
                BEGIN INSERT INTO thread_fact_audit(fact, operation) VALUES ('trace', 'delete'); END;
                CREATE TRIGGER audit_submission_insert AFTER INSERT ON thread_submissions
                BEGIN INSERT INTO thread_fact_audit(fact, operation) VALUES ('submission', 'insert'); END;
                CREATE TRIGGER audit_submission_update AFTER UPDATE ON thread_submissions
                BEGIN INSERT INTO thread_fact_audit(fact, operation) VALUES ('submission', 'update'); END;
                CREATE TRIGGER audit_submission_delete AFTER DELETE ON thread_submissions
                BEGIN INSERT INTO thread_fact_audit(fact, operation) VALUES ('submission', 'delete'); END;
                "#,
            )
            .expect("install fact audit triggers");

        let mut initial = commit("thread-a", None, 1, None, None);
        initial.runtime_events.push(StoredThreadRuntimeEvent {
            sequence: 1,
            created_at: 10,
            payload: serde_json::json!({ "value": 1 }),
        });
        initial.trace_events.push(StoredThreadTraceEvent {
            sequence: 1,
            payload: serde_json::json!({ "value": 1 }),
        });
        initial.submissions.push(StoredThreadSubmission {
            thread_id: "thread-a".to_string(),
            ordinal: 1,
            created_at: 10,
            submission: serde_json::json!({ "value": 1 }),
        });
        assert_eq!(
            store
                .commit_thread_runtime(initial)
                .await
                .expect("initial facts"),
            ThreadRuntimeCommitOutcome::Applied
        );
        let baseline: i64 = connection
            .query_row(
                "SELECT COALESCE(MAX(sequence), 0) FROM thread_fact_audit",
                [],
                |row| row.get(0),
            )
            .expect("audit baseline");

        let mut replay = commit("thread-a", Some(1), 2, None, None);
        replay.runtime_events.push(StoredThreadRuntimeEvent {
            sequence: 1,
            created_at: 11,
            payload: serde_json::json!({ "value": 2 }),
        });
        replay.trace_events.push(StoredThreadTraceEvent {
            sequence: 1,
            payload: serde_json::json!({ "value": 2 }),
        });
        replay.submissions.push(StoredThreadSubmission {
            thread_id: "thread-a".to_string(),
            ordinal: 1,
            created_at: 11,
            submission: serde_json::json!({ "value": 2 }),
        });
        assert_eq!(
            store
                .commit_thread_runtime(replay)
                .await
                .expect("replayed facts"),
            ThreadRuntimeCommitOutcome::Applied
        );

        let mut statement = connection
            .prepare(
                "SELECT fact, operation FROM thread_fact_audit \
                 WHERE sequence > ?1 ORDER BY sequence",
            )
            .expect("prepare audit query");
        let writes = statement
            .query_map([baseline], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query audit")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect audit rows");
        assert_eq!(
            writes,
            vec![
                ("event".to_string(), "update".to_string()),
                ("trace".to_string(), "update".to_string()),
                ("submission".to_string(), "update".to_string()),
            ]
        );
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
            submissions: Vec::new(),
        }
    }

    fn turn(id: &str, thread_id: &str, timestamp: i64) -> Turn {
        Turn {
            id: id.to_string(),
            thread_id: thread_id.to_string(),
            revision: 1,
            state: TurnState::Completed(CompletedTurnState::new(
                Some(timestamp),
                timestamp,
                TurnCompletion::Normal,
            )),
            updated_at: timestamp,
        }
    }
}
