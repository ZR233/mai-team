use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use super::ThreadRuntimeCommitOutcome;
use crate::{Result, StoreError, i64_to_u64};

mod prepare;

const THREAD_WRITER_BUSY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(super) struct PreparedThreadCommit {
    expected_revision: Option<u64>,
    runtime: PreparedRuntime,
    items: Option<Vec<PreparedItem>>,
    turn: Option<PreparedTurn>,
    notifications: Vec<PreparedNotification>,
    runtime_events: Vec<PreparedRuntimeEvent>,
    trace_events: Vec<PreparedTraceEvent>,
    submissions: Vec<PreparedSubmission>,
}

#[derive(Debug)]
struct PreparedRuntime {
    thread_id: String,
    revision: i64,
    document_json: String,
    snapshot_json: Option<String>,
    updated_at: i64,
}

#[derive(Debug)]
struct PreparedItem {
    id: String,
    thread_id: String,
    turn_id: String,
    ordinal: i64,
    revision: i64,
    item_json: String,
}

#[derive(Debug)]
struct PreparedTurn {
    id: String,
    thread_id: String,
    ordinal: Option<i64>,
    turn_json: Option<String>,
    model_json: Option<String>,
    initial_context_disposition: String,
}

#[derive(Debug)]
struct PreparedNotification {
    id: String,
    thread_id: String,
    revision: i64,
    emitted_at: i64,
    notification_json: String,
}

#[derive(Debug)]
struct PreparedRuntimeEvent {
    id: String,
    thread_id: String,
    sequence: i64,
    created_at: i64,
    event_json: String,
}

#[derive(Debug)]
struct PreparedTraceEvent {
    id: String,
    thread_id: String,
    sequence: i64,
    trace_json: String,
}

#[derive(Debug)]
struct PreparedSubmission {
    id: String,
    thread_id: String,
    ordinal: i64,
    created_at: i64,
    submission_json: String,
}

#[derive(Debug)]
struct ExistingRuntime {
    revision: i64,
    snapshot_json: Option<String>,
}

#[derive(Debug)]
struct ExistingItem {
    thread_id: String,
    turn_id: String,
    ordinal: i64,
    revision: i64,
    item_json: String,
}

#[derive(Debug)]
struct ExistingTurn {
    thread_id: String,
    ordinal: i64,
    turn_json: String,
    model_json: Option<String>,
    context_disposition: String,
}

impl PreparedThreadCommit {
    pub(super) fn commit_on_path(&self, path: &Path) -> Result<ThreadRuntimeCommitOutcome> {
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(THREAD_WRITER_BUSY_TIMEOUT)?;
        // Thread writer 是同一进程内的单一所有者。先用 deferred transaction 完成
        // revision 与 Item 差异读取，到首次写入时再竞争 SQLite writer，避免大 Thread
        // 的全量比较阶段长期占住全库唯一写锁。
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let existing = transaction
            .query_row(
                "SELECT revision, snapshot_json FROM thread_runtime_documents WHERE thread_id = ?1",
                params![self.runtime.thread_id],
                |row| {
                    Ok(ExistingRuntime {
                        revision: row.get(0)?,
                        snapshot_json: row.get(1)?,
                    })
                },
            )
            .optional()?;
        let actual_revision = existing.as_ref().map(|row| i64_to_u64(row.revision));
        if actual_revision != self.expected_revision {
            return Ok(ThreadRuntimeCommitOutcome::RevisionConflict { actual_revision });
        }

        let snapshot_json = self
            .runtime
            .snapshot_json
            .as_ref()
            .or_else(|| existing.as_ref().and_then(|row| row.snapshot_json.as_ref()));
        transaction.execute(
            "INSERT INTO thread_runtime_documents \
                (thread_id, revision, document_json, snapshot_json, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(thread_id) DO UPDATE SET \
                revision = excluded.revision, document_json = excluded.document_json, \
                snapshot_json = excluded.snapshot_json, updated_at = excluded.updated_at",
            params![
                self.runtime.thread_id,
                self.runtime.revision,
                self.runtime.document_json,
                snapshot_json,
                self.runtime.updated_at,
            ],
        )?;

        if let Some(items) = &self.items {
            let mut existing_items = {
                let mut statement = transaction.prepare(
                    "SELECT id, thread_id, turn_id, ordinal, revision, item_json \
                     FROM thread_items WHERE thread_id = ?1",
                )?;
                statement
                    .query_map(params![self.runtime.thread_id], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            ExistingItem {
                                thread_id: row.get(1)?,
                                turn_id: row.get(2)?,
                                ordinal: row.get(3)?,
                                revision: row.get(4)?,
                                item_json: row.get(5)?,
                            },
                        ))
                    })?
                    .collect::<rusqlite::Result<BTreeMap<_, _>>>()?
            };
            {
                let mut statement = transaction.prepare(
                    "INSERT INTO thread_items \
                        (id, thread_id, turn_id, ordinal, revision, item_json) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                     ON CONFLICT(id) DO UPDATE SET \
                        thread_id = excluded.thread_id, turn_id = excluded.turn_id, \
                        ordinal = excluded.ordinal, revision = excluded.revision, \
                        item_json = excluded.item_json",
                )?;
                for item in items {
                    let unchanged = existing_items.remove(&item.id).is_some_and(|existing| {
                        existing.thread_id == item.thread_id
                            && existing.turn_id == item.turn_id
                            && existing.ordinal == item.ordinal
                            && existing.revision == item.revision
                            && existing.item_json == item.item_json
                    });
                    if !unchanged {
                        statement.execute(params![
                            item.id,
                            item.thread_id,
                            item.turn_id,
                            item.ordinal,
                            item.revision,
                            item.item_json,
                        ])?;
                    }
                }
            }
            let mut statement = transaction.prepare("DELETE FROM thread_items WHERE id = ?1")?;
            for item_id in existing_items.into_keys() {
                statement.execute(params![item_id])?;
            }
        }

        if let Some(update) = &self.turn {
            let previous = transaction
                .query_row(
                    "SELECT thread_id, ordinal, turn_json, model_json, context_disposition \
                     FROM thread_turns WHERE id = ?1",
                    params![update.id],
                    |row| {
                        Ok(ExistingTurn {
                            thread_id: row.get(0)?,
                            ordinal: row.get(1)?,
                            turn_json: row.get(2)?,
                            model_json: row.get(3)?,
                            context_disposition: row.get(4)?,
                        })
                    },
                )
                .optional()?;
            if previous
                .as_ref()
                .is_some_and(|record| record.thread_id != update.thread_id)
            {
                return Err(StoreError::InvalidConfig(format!(
                    "Turn {} cannot move from another Thread to {}",
                    update.id, update.thread_id
                )));
            }
            let turn_json = update
                .turn_json
                .as_ref()
                .or_else(|| previous.as_ref().map(|record| &record.turn_json))
                .ok_or_else(|| {
                    StoreError::InvalidConfig(format!(
                        "Turn billing commit {} has no durable Turn",
                        update.id
                    ))
                })?;
            let model_json = update.model_json.as_ref().or_else(|| {
                previous
                    .as_ref()
                    .and_then(|record| record.model_json.as_ref())
            });
            let ordinal = update
                .ordinal
                .or_else(|| previous.as_ref().map(|record| record.ordinal))
                .ok_or_else(|| {
                    StoreError::InvalidConfig(format!("Turn {} lacks ordinal", update.id))
                })?;
            let context_disposition = previous
                .as_ref()
                .map(|record| &record.context_disposition)
                .unwrap_or(&update.initial_context_disposition);
            transaction.execute(
                "INSERT INTO thread_turns \
                    (id, thread_id, ordinal, turn_json, model_json, context_disposition) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(id) DO UPDATE SET \
                    thread_id = excluded.thread_id, ordinal = excluded.ordinal, \
                    turn_json = excluded.turn_json, model_json = excluded.model_json, \
                    context_disposition = excluded.context_disposition",
                params![
                    update.id,
                    update.thread_id,
                    ordinal,
                    turn_json,
                    model_json,
                    context_disposition,
                ],
            )?;
        }

        {
            let mut statement = transaction.prepare(
                "INSERT INTO thread_notifications \
                    (id, thread_id, revision, emitted_at, notification_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(id) DO UPDATE SET \
                    thread_id = excluded.thread_id, revision = excluded.revision, \
                    emitted_at = excluded.emitted_at, notification_json = excluded.notification_json",
            )?;
            for notification in &self.notifications {
                statement.execute(params![
                    notification.id,
                    notification.thread_id,
                    notification.revision,
                    notification.emitted_at,
                    notification.notification_json,
                ])?;
            }
        }
        {
            let mut statement = transaction.prepare(
                "INSERT INTO thread_runtime_events \
                    (id, thread_id, sequence, created_at, event_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(id) DO UPDATE SET \
                    thread_id = excluded.thread_id, sequence = excluded.sequence, \
                    created_at = excluded.created_at, event_json = excluded.event_json",
            )?;
            for event in &self.runtime_events {
                statement.execute(params![
                    event.id,
                    event.thread_id,
                    event.sequence,
                    event.created_at,
                    event.event_json,
                ])?;
            }
        }
        {
            let mut statement = transaction.prepare(
                "INSERT INTO thread_runtime_traces \
                    (id, thread_id, sequence, trace_json) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(id) DO UPDATE SET \
                    thread_id = excluded.thread_id, sequence = excluded.sequence, \
                    trace_json = excluded.trace_json",
            )?;
            for event in &self.trace_events {
                statement.execute(params![
                    event.id,
                    event.thread_id,
                    event.sequence,
                    event.trace_json,
                ])?;
            }
        }
        {
            let mut statement = transaction.prepare(
                "INSERT INTO thread_submissions \
                    (id, thread_id, ordinal, created_at, submission_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(id) DO UPDATE SET \
                    thread_id = excluded.thread_id, ordinal = excluded.ordinal, \
                    created_at = excluded.created_at, submission_json = excluded.submission_json",
            )?;
            for submission in &self.submissions {
                statement.execute(params![
                    submission.id,
                    submission.thread_id,
                    submission.ordinal,
                    submission.created_at,
                    submission.submission_json,
                ])?;
            }
        }

        transaction.commit()?;
        Ok(ThreadRuntimeCommitOutcome::Applied)
    }
}
