use mai_protocol::ThreadContextDisposition;
use std::collections::BTreeSet;

use super::super::ThreadRuntimeCommitDocument;
use super::*;
use crate::{Result, StoreError, u64_to_i64};

impl PreparedThreadCommit {
    pub(in crate::thread_runtime) fn try_new(
        document: ThreadRuntimeCommitDocument,
    ) -> Result<Self> {
        let thread_id = document.runtime.thread_id.clone();
        let items = document
            .runtime
            .snapshot
            .as_ref()
            .map(|snapshot| {
                if snapshot.thread.id != thread_id {
                    return Err(StoreError::InvalidConfig(format!(
                        "Thread snapshot {} does not belong to runtime {thread_id}",
                        snapshot.thread.id
                    )));
                }
                let mut seen = BTreeSet::new();
                snapshot
                    .items
                    .iter()
                    .map(|item| {
                        if item.thread_id != thread_id {
                            return Err(StoreError::InvalidConfig(format!(
                                "Thread Item {} belongs to {} instead of runtime {thread_id}",
                                item.id, item.thread_id
                            )));
                        }
                        if !seen.insert(item.id.as_str()) {
                            return Err(StoreError::InvalidConfig(format!(
                                "Thread snapshot {thread_id} contains duplicate Item {}",
                                item.id
                            )));
                        }
                        Ok(PreparedItem {
                            id: item.id.clone(),
                            thread_id: item.thread_id.clone(),
                            turn_id: item.turn_id.clone(),
                            ordinal: u64_to_i64(item.ordinal),
                            revision: u64_to_i64(item.revision),
                            item_json: serde_json::to_string(item)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;

        let turn = document
            .turn
            .as_ref()
            .map(|update| {
                if update.thread_id != thread_id {
                    return Err(StoreError::InvalidConfig(format!(
                        "Turn commit {} belongs to {} instead of runtime {thread_id}",
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
                Ok(PreparedTurn {
                    id: update.id.clone(),
                    thread_id: update.thread_id.clone(),
                    ordinal: update
                        .turn
                        .as_ref()
                        .map(|turn| turn.started_at().unwrap_or(turn.updated_at)),
                    turn_json: update
                        .turn
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                    model_json: update
                        .billing
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                    initial_context_disposition: serde_json::to_string(
                        &ThreadContextDisposition::Active,
                    )?,
                })
            })
            .transpose()?;

        let notifications = document
            .notifications
            .iter()
            .map(|notification| {
                if notification.thread_id != thread_id {
                    return Err(StoreError::InvalidConfig(format!(
                        "Thread notification {} belongs to {} instead of runtime {thread_id}",
                        notification.revision, notification.thread_id
                    )));
                }
                Ok(PreparedNotification {
                    id: format!("{thread_id}:{}", notification.revision),
                    thread_id: thread_id.clone(),
                    revision: u64_to_i64(notification.revision),
                    emitted_at: notification.emitted_at,
                    notification_json: serde_json::to_string(notification)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let runtime_events = document
            .runtime_events
            .iter()
            .map(|event| {
                Ok(PreparedRuntimeEvent {
                    id: format!("{thread_id}:{}", event.sequence),
                    thread_id: thread_id.clone(),
                    sequence: u64_to_i64(event.sequence),
                    created_at: event.created_at,
                    event_json: serde_json::to_string(&event.payload)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let trace_events = document
            .trace_events
            .iter()
            .map(|event| {
                Ok(PreparedTraceEvent {
                    id: format!("{thread_id}:{}", event.sequence),
                    thread_id: thread_id.clone(),
                    sequence: u64_to_i64(event.sequence),
                    trace_json: serde_json::to_string(&event.payload)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let submissions = document
            .submissions
            .iter()
            .map(|submission| {
                if submission.thread_id != thread_id {
                    return Err(StoreError::InvalidConfig(format!(
                        "Thread submission {} belongs to {} instead of runtime {thread_id}",
                        submission.ordinal, submission.thread_id
                    )));
                }
                Ok(PreparedSubmission {
                    id: format!("{thread_id}:{}", submission.ordinal),
                    thread_id: thread_id.clone(),
                    ordinal: u64_to_i64(submission.ordinal),
                    created_at: submission.created_at,
                    submission_json: serde_json::to_string(&submission.submission)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            expected_revision: document.expected_revision,
            runtime: PreparedRuntime {
                thread_id,
                revision: u64_to_i64(document.runtime.revision),
                document_json: serde_json::to_string(&document.runtime.document)?,
                snapshot_json: document
                    .runtime
                    .snapshot
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                updated_at: document.runtime.updated_at,
            },
            items,
            turn,
            notifications,
            runtime_events,
            trace_events,
            submissions,
        })
    }
}
