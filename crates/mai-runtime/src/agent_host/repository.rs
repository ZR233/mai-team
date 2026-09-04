use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mai_store::{
    MaiStore, StoredThreadRuntime, StoredThreadRuntimeEvent, StoredThreadSubmission,
    StoredThreadTraceEvent, ThreadRuntimeCommitDocument,
    ThreadRuntimeCommitOutcome as StoreCommitOutcome, ThreadRuntimeTurnCommit,
    is_retryable_sqlite_error,
};
use pl_core::{
    AgentSession, AgentSnapshot, AgentSubmissionPage, AgentSubmissionRecord,
    DurableMailboxEnvelope, RestoredAgentRuntime, RestoredThreadSnapshot, ThreadActorState,
    ThreadCommit, ThreadContextState, ThreadId, ThreadRepository,
};
use pl_model::TokenUsage;
use pl_protocol::{AgentSessionSnapshot, TurnBillingRecord};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::{Result, RuntimeError};

/// 使用 mai-store transaction 实现的 PL canonical Thread repository。
#[derive(Clone)]
pub(crate) struct MaiAgentRepository {
    store: Arc<MaiStore>,
    writer: Arc<ThreadWriter>,
}

impl MaiAgentRepository {
    pub(crate) fn new(store: Arc<MaiStore>) -> Self {
        Self {
            writer: Arc::new(ThreadWriter::new(store.clone())),
            store,
        }
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.writer.shutdown().await
    }

    pub(crate) async fn wait_for_failure(&self) -> RuntimeError {
        self.writer.wait_for_failure().await
    }
}

impl ThreadRepository for MaiAgentRepository {
    type Error = RuntimeError;

    async fn restore_runtime(&self) -> Result<Vec<RestoredAgentRuntime>> {
        // mai 没有启动钉住集合；产品 Agent 身份先恢复，PL runtime 在首次访问时驻留。
        Ok(Vec::new())
    }

    async fn restore_thread(&self, thread_id: &ThreadId) -> Result<Option<RestoredAgentRuntime>> {
        let restored = self
            .store
            .load_thread_runtime(&thread_id.to_string())
            .await?
            .map(runtime_from_store)
            .transpose()?;
        if let Some(runtime) = &restored {
            self.writer.seed(thread_id, runtime.state.snapshot.revision);
        }
        Ok(restored)
    }
    async fn commit(&self, commit: ThreadCommit) -> Result<()> {
        self.writer.enqueue(commit).await
    }

    async fn await_durable(&self, thread_id: &ThreadId, revision: u64) -> Result<()> {
        self.writer.await_durable(thread_id, revision).await
    }

    fn pending_commit_count(&self) -> usize {
        self.writer.pending.load(Ordering::Acquire)
    }

    async fn list_submissions(
        &self,
        thread_id: &ThreadId,
        offset: usize,
        limit: usize,
    ) -> Result<AgentSubmissionPage> {
        let page = self
            .store
            .list_thread_submissions(&thread_id.to_string(), offset, limit)
            .await?;
        let items = page
            .items
            .iter()
            .map(|item| serde_json::from_value::<AgentSubmissionRecord>(item.submission.clone()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(json_error)?;
        Ok(AgentSubmissionPage {
            items,
            offset: page.offset,
            limit: page.limit,
            total: page.total,
            has_more: page.has_more,
        })
    }
}

const MAX_PENDING_COMMITS: usize = 1024;

struct QueuedCommit {
    commit: Box<ThreadCommit>,
}

struct ThreadWriter {
    store: Arc<MaiStore>,
    queue: Mutex<VecDeque<QueuedCommit>>,
    durable: Mutex<HashMap<String, u64>>,
    failure: watch::Sender<Option<String>>,
    task: Mutex<Option<JoinHandle<()>>>,
    work: Notify,
    progress: Notify,
    pending: AtomicUsize,
    stopping: AtomicBool,
}

impl ThreadWriter {
    fn new(store: Arc<MaiStore>) -> Self {
        Self {
            store,
            queue: Mutex::new(VecDeque::new()),
            durable: Mutex::new(HashMap::new()),
            failure: watch::Sender::new(None),
            task: Mutex::new(None),
            work: Notify::new(),
            progress: Notify::new(),
            pending: AtomicUsize::new(0),
            stopping: AtomicBool::new(false),
        }
    }

    fn seed(&self, thread_id: &ThreadId, revision: u64) {
        let mut durable = self
            .durable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        durable
            .entry(thread_id.to_string())
            .and_modify(|current| *current = (*current).max(revision))
            .or_insert(revision);
    }

    async fn enqueue(self: &Arc<Self>, commit: ThreadCommit) -> Result<()> {
        self.ensure_running()?;
        let mut commit = Some(Box::new(commit));
        loop {
            self.check_available()?;
            let progress = self.progress.notified();
            {
                let mut queue = self
                    .queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let next = commit.take().expect("Thread commit present");
                if let Some(previous) = queue.back_mut() {
                    match previous.commit.coalesce(next) {
                        Ok(()) => {
                            drop(queue);
                            self.work.notify_one();
                            return Ok(());
                        }
                        Err(next) => commit = Some(next),
                    }
                } else {
                    commit = Some(next);
                }
                if self.pending.load(Ordering::Acquire) < MAX_PENDING_COMMITS {
                    queue.push_back(QueuedCommit {
                        commit: commit.take().expect("Thread commit present"),
                    });
                    self.pending.fetch_add(1, Ordering::AcqRel);
                    drop(queue);
                    self.work.notify_one();
                    return Ok(());
                }
            }
            progress.await;
        }
    }

    async fn await_durable(self: &Arc<Self>, thread_id: &ThreadId, revision: u64) -> Result<()> {
        loop {
            if self.durable_revision(thread_id) >= revision {
                return Ok(());
            }
            self.check_available()?;
            self.ensure_running()?;
            let progress = self.progress.notified();
            if self.durable_revision(thread_id) >= revision {
                return Ok(());
            }
            self.work.notify_one();
            progress.await;
        }
    }

    async fn shutdown(self: &Arc<Self>) -> Result<()> {
        self.stopping.store(true, Ordering::Release);
        self.ensure_task();
        self.work.notify_waiters();
        let task = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            task.await.map_err(|error| {
                RuntimeError::InvalidInput(format!("Thread writer task failed: {error}"))
            })?;
        }
        self.check_failure()?;
        if self.pending.load(Ordering::Acquire) != 0 {
            return Err(RuntimeError::InvalidInput(
                "Thread writer stopped before its queue drained".to_string(),
            ));
        }
        Ok(())
    }

    fn durable_revision(&self, thread_id: &ThreadId) -> u64 {
        self.durable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(thread_id.as_str())
            .copied()
            .unwrap_or(0)
    }

    fn ensure_running(self: &Arc<Self>) -> Result<()> {
        self.check_available()?;
        self.ensure_task();
        Ok(())
    }

    fn ensure_task(self: &Arc<Self>) {
        let mut task = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if task.is_none() {
            let writer = self.clone();
            *task = Some(tokio::spawn(async move { writer.run().await }));
        }
    }

    async fn run(self: Arc<Self>) {
        loop {
            let work = self.work.notified();
            let commit = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front();
            let Some(commit) = commit else {
                if self.stopping.load(Ordering::Acquire) {
                    return;
                }
                work.await;
                continue;
            };
            let thread_id = commit.commit.agent_id.to_string();
            let revision = commit.commit.facts.revision;
            let document = match commit_to_store((*commit.commit).clone()) {
                Ok(document) => document,
                Err(error) => {
                    self.requeue_failed(commit, error.to_string());
                    return;
                }
            };
            match self.store.commit_thread_runtime(document).await {
                Ok(StoreCommitOutcome::Applied) => {
                    self.advance_durable(&thread_id, revision);
                    self.pending.fetch_sub(1, Ordering::AcqRel);
                    self.progress.notify_waiters();
                }
                Ok(StoreCommitOutcome::RevisionConflict { actual_revision }) => {
                    self.requeue_failed(
                        commit,
                        format!(
                            "Thread persistence revision conflict, durable revision is {actual_revision:?}"
                        ),
                    );
                    return;
                }
                Err(error) if is_retryable_sqlite_error(&error) => {
                    if self.stopping.load(Ordering::Acquire) {
                        self.requeue_failed(commit, error.to_string());
                        return;
                    }
                    self.requeue_retry(commit);
                    tracing::warn!(
                        thread_id,
                        revision,
                        "Thread writer 遇到临时 SQLite 锁，保留队首提交后重试"
                    );
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(error) => {
                    self.requeue_failed(commit, error.to_string());
                    return;
                }
            }
        }
    }

    fn advance_durable(&self, thread_id: &str, revision: u64) {
        let mut durable = self
            .durable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        durable
            .entry(thread_id.to_string())
            .and_modify(|current| *current = (*current).max(revision))
            .or_insert(revision);
    }

    fn requeue_failed(&self, commit: QueuedCommit, error: String) {
        self.requeue_retry(commit);
        self.failure.send_replace(Some(error));
        self.progress.notify_waiters();
    }

    fn requeue_retry(&self, commit: QueuedCommit) {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_front(commit);
    }

    fn check_available(&self) -> Result<()> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(RuntimeError::InvalidInput(
                "Thread writer is shutting down".to_string(),
            ));
        }
        self.check_failure()
    }

    fn check_failure(&self) -> Result<()> {
        if let Some(error) = self.failure_message() {
            return Err(RuntimeError::InvalidInput(format!(
                "Thread writer is blocked: {error}"
            )));
        }
        Ok(())
    }

    async fn wait_for_failure(&self) -> RuntimeError {
        let mut failure = self.failure.subscribe();
        loop {
            if let Some(error) = failure.borrow_and_update().clone() {
                return RuntimeError::InvalidInput(format!("Thread writer is blocked: {error}"));
            }
            failure
                .changed()
                .await
                .expect("Thread writer failure sender lives as long as its receiver");
        }
    }

    fn failure_message(&self) -> Option<String> {
        self.failure.borrow().clone()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredThreadActorDocument {
    snapshot: AgentSnapshot,
    context: StoredThreadContextDocument,
    pending_inputs: Vec<DurableMailboxEnvelope>,
    active_input: Option<DurableMailboxEnvelope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredThreadContextDocument {
    metadata: pl_core::ThreadContextMetadata,
    session: AgentSessionSnapshot,
    usage: TokenUsage,
    billing_by_turn: BTreeMap<String, TurnBillingRecord>,
    last_context_tokens: Option<u64>,
    trace_sequence: u64,
    thread_revision: u64,
}

fn commit_to_store(commit: ThreadCommit) -> Result<ThreadRuntimeCommitDocument> {
    let ThreadCommit {
        agent_id,
        persistence: _,
        expected_revision,
        next_state,
        facts,
        mutation: _,
    } = commit;
    let billing = facts.inference.as_ref().and_then(|_| {
        facts.turn_id.as_ref().and_then(|turn_id| {
            next_state
                .session
                .billing_by_turn
                .get(turn_id.as_str())
                .cloned()
        })
    });
    let turn = match (facts.turn_id.as_ref(), facts.turn_transition, billing) {
        (Some(_), None, None) => None,
        (Some(turn_id), transition, billing) => Some(ThreadRuntimeTurnCommit {
            id: turn_id.to_string(),
            thread_id: facts.thread_id.to_string(),
            turn: transition,
            billing,
        }),
        (None, None, None) => None,
        (None, Some(_), None) | (None, None, Some(_)) | (None, Some(_), Some(_)) => {
            return Err(RuntimeError::InvalidInput(
                "Thread commit contains a Turn transition without Turn id".to_string(),
            ));
        }
    };
    let document = actor_document(&next_state);
    let runtime_events = facts
        .runtime_events
        .into_iter()
        .map(|event| {
            Ok(StoredThreadRuntimeEvent {
                sequence: event.sequence,
                created_at: event.created_at,
                payload: serde_json::to_value(event).map_err(json_error)?,
            })
        })
        .collect::<Result<_>>()?;
    let trace_events = facts
        .trace_events
        .into_iter()
        .map(|event| {
            Ok(StoredThreadTraceEvent {
                sequence: event.sequence,
                payload: serde_json::to_value(event).map_err(json_error)?,
            })
        })
        .collect::<Result<_>>()?;
    let submissions = facts
        .submission
        .as_ref()
        .map(|submission| -> Result<Vec<StoredThreadSubmission>> {
            Ok(vec![StoredThreadSubmission {
                thread_id: facts.thread_id.to_string(),
                ordinal: facts.revision,
                created_at: submission.created_at,
                submission: serde_json::to_value(AgentSubmissionRecord::from(submission))
                    .map_err(json_error)?,
            }])
        })
        .transpose()?
        .unwrap_or_default();
    Ok(ThreadRuntimeCommitDocument {
        expected_revision,
        runtime: StoredThreadRuntime {
            thread_id: agent_id.to_string(),
            revision: next_state.snapshot.revision,
            document: serde_json::to_value(document).map_err(json_error)?,
            snapshot: facts.projection_snapshot,
            updated_at: next_state.snapshot.updated_at,
        },
        turn,
        notifications: facts.notifications,
        runtime_events,
        trace_events,
        submissions,
    })
}

fn runtime_from_store(runtime: StoredThreadRuntime) -> Result<RestoredAgentRuntime> {
    let document = serde_json::from_value::<StoredThreadActorDocument>(runtime.document)
        .map_err(json_error)?;
    Ok(RestoredAgentRuntime {
        state: ThreadActorState {
            snapshot: document.snapshot,
            session: ThreadContextState {
                metadata: document.context.metadata,
                session: AgentSession::from_snapshot(document.context.session),
                usage: document.context.usage,
                billing_by_turn: document.context.billing_by_turn,
                last_context_tokens: document.context.last_context_tokens,
                trace_sequence: document.context.trace_sequence,
                thread_revision: document.context.thread_revision,
            },
            pending_inputs: VecDeque::from(document.pending_inputs),
            active_input: document.active_input,
        },
        thread_snapshot: runtime
            .snapshot
            .map(|snapshot| RestoredThreadSnapshot { snapshot }),
    })
}

fn actor_document(state: &ThreadActorState) -> StoredThreadActorDocument {
    StoredThreadActorDocument {
        snapshot: state.snapshot.clone(),
        context: StoredThreadContextDocument {
            metadata: state.session.metadata.clone(),
            session: state.session.session.snapshot(),
            usage: state.session.usage.clone(),
            billing_by_turn: state.session.billing_by_turn.clone(),
            last_context_tokens: state.session.last_context_tokens,
            trace_sequence: state.session.trace_sequence,
            thread_revision: state.session.thread_revision,
        },
        pending_inputs: state.pending_inputs.iter().cloned().collect(),
        active_input: state.active_input.clone(),
    }
}

fn json_error(error: serde_json::Error) -> RuntimeError {
    RuntimeError::InvalidInput(format!("invalid Thread repository document: {error}"))
}

#[cfg(test)]
mod tests {
    use pl_core::{
        AgentIdentity, AgentRoleId, AgentState, DurableCommitFacts, PersistenceClass,
        ThreadMutation,
    };
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn runtime_restore_is_empty_until_one_thread_is_requested() {
        let (_directory, store) = test_store().await;
        let thread_id = ThreadId::new("thread-lazy").expect("thread id");
        let snapshot = AgentSnapshot {
            identity: AgentIdentity {
                id: thread_id.clone(),
                parent_id: None,
                role: AgentRoleId::new("executor").expect("role"),
                depth: 0,
            },
            state: AgentState::idle(),
            pending_inputs: 0,
            progress: None,
            last_turn: None,
            revision: 7,
            event_sequence: 11,
            updated_at: 42,
        };
        let state = ThreadActorState {
            snapshot: snapshot.clone(),
            session: ThreadContextState::empty(),
            pending_inputs: VecDeque::new(),
            active_input: None,
        };
        assert_eq!(
            store
                .commit_thread_runtime(ThreadRuntimeCommitDocument {
                    expected_revision: None,
                    runtime: StoredThreadRuntime {
                        thread_id: thread_id.to_string(),
                        revision: snapshot.revision,
                        document: serde_json::to_value(actor_document(&state))
                            .expect("actor document"),
                        snapshot: None,
                        updated_at: snapshot.updated_at,
                    },
                    turn: None,
                    notifications: Vec::new(),
                    runtime_events: Vec::new(),
                    trace_events: Vec::new(),
                    submissions: Vec::new(),
                })
                .await
                .expect("persist runtime"),
            StoreCommitOutcome::Applied
        );

        let repository = MaiAgentRepository::new(store);
        assert!(
            repository
                .restore_runtime()
                .await
                .expect("restore pinned runtime")
                .is_empty()
        );

        let restored = repository
            .restore_thread(&thread_id)
            .await
            .expect("restore one thread")
            .expect("stored thread");
        assert_eq!(restored.state.snapshot, snapshot);
        assert!(restored.state.pending_inputs.is_empty());
        assert!(restored.state.active_input.is_none());
        repository
            .await_durable(&thread_id, 7)
            .await
            .expect("restored revision seeds the exact barrier");
        repository.shutdown().await.expect("shutdown repository");
    }

    #[tokio::test]
    async fn exact_revision_barrier_waits_for_fifo_persistence() {
        let (_directory, store) = test_store().await;
        let writer = Arc::new(ThreadWriter::new(store.clone()));
        let thread_id = ThreadId::new("thread-a").expect("thread id");

        writer
            .enqueue(commit("thread-a", None, 1, PersistenceClass::Standard))
            .await
            .expect("enqueue revision 1");
        writer
            .enqueue(commit("thread-a", Some(1), 2, PersistenceClass::Standard))
            .await
            .expect("enqueue revision 2");
        writer
            .await_durable(&thread_id, 2)
            .await
            .expect("revision 2 durable");

        assert_eq!(writer.pending.load(Ordering::Acquire), 0);
        assert_eq!(
            store
                .load_thread_runtime("thread-a")
                .await
                .expect("load runtime")
                .expect("runtime")
                .revision,
            2
        );
        writer.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn revision_conflict_blocks_exact_barrier_without_losing_commit() {
        let (_directory, store) = test_store().await;
        let writer = Arc::new(ThreadWriter::new(store.clone()));
        let thread_id = ThreadId::new("thread-a").expect("thread id");

        writer
            .enqueue(commit("thread-a", None, 1, PersistenceClass::Standard))
            .await
            .expect("enqueue revision 1");
        writer
            .await_durable(&thread_id, 1)
            .await
            .expect("revision 1 durable");
        let failure_waiter = {
            let writer = Arc::clone(&writer);
            tokio::spawn(async move { writer.wait_for_failure().await })
        };
        writer
            .enqueue(commit("thread-a", None, 2, PersistenceClass::Standard))
            .await
            .expect("enqueue conflicting revision");

        let error = writer
            .await_durable(&thread_id, 2)
            .await
            .expect_err("conflict must fail the barrier");
        assert!(error.to_string().contains("revision conflict"));
        let signalled = tokio::time::timeout(Duration::from_secs(1), failure_waiter)
            .await
            .expect("fatal writer failure must wake its supervisor")
            .expect("failure waiter task");
        assert!(signalled.to_string().contains("revision conflict"));
        let late_signalled =
            tokio::time::timeout(Duration::from_secs(1), writer.wait_for_failure())
                .await
                .expect("late supervisor must observe the retained fatal failure");
        assert!(late_signalled.to_string().contains("revision conflict"));
        assert_eq!(writer.pending.load(Ordering::Acquire), 1);
        assert_eq!(
            store
                .load_thread_runtime("thread-a")
                .await
                .expect("load runtime")
                .expect("runtime")
                .revision,
            1
        );
        assert!(writer.shutdown().await.is_err());
    }

    #[tokio::test]
    async fn shutdown_drains_all_accepted_commits_and_rejects_new_work() {
        let (_directory, store) = test_store().await;
        let writer = Arc::new(ThreadWriter::new(store.clone()));
        for revision in 1..=16 {
            writer
                .enqueue(commit(
                    "thread-a",
                    (revision > 1).then_some(revision - 1),
                    revision,
                    PersistenceClass::Standard,
                ))
                .await
                .expect("enqueue sequential revision");
        }

        writer.shutdown().await.expect("drain writer");

        assert_eq!(writer.pending.load(Ordering::Acquire), 0);
        assert_eq!(
            store
                .load_thread_runtime("thread-a")
                .await
                .expect("load runtime")
                .expect("runtime")
                .revision,
            16
        );
        assert!(
            writer
                .enqueue(commit("thread-a", Some(16), 17, PersistenceClass::Standard,))
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn consecutive_streaming_commits_share_one_durable_write() {
        let (_directory, store) = test_store().await;
        let writer = Arc::new(ThreadWriter::new(store.clone()));
        let thread_id = ThreadId::new("thread-stream").expect("thread id");

        writer
            .enqueue(commit(
                "thread-stream",
                None,
                1,
                PersistenceClass::Coalescible,
            ))
            .await
            .expect("enqueue first stream commit");
        writer
            .enqueue(commit(
                "thread-stream",
                Some(1),
                2,
                PersistenceClass::Coalescible,
            ))
            .await
            .expect("enqueue next stream commit");

        assert_eq!(writer.pending.load(Ordering::Acquire), 1);
        writer
            .await_durable(&thread_id, 2)
            .await
            .expect("coalesced revision durable");
        assert_eq!(
            store
                .load_thread_runtime("thread-stream")
                .await
                .expect("load runtime")
                .expect("runtime")
                .revision,
            2
        );
        writer.shutdown().await.expect("shutdown");
    }

    async fn test_store() -> (tempfile::TempDir, Arc<MaiStore>) {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = MaiStore::open_with_config_and_artifact_index_path(
            directory.path().join("store.sqlite3"),
            directory.path().join("config.toml"),
            directory.path().join("artifacts"),
        )
        .await
        .expect("open store");
        (directory, Arc::new(store))
    }

    fn commit(
        thread_id: &str,
        expected_revision: Option<u64>,
        revision: u64,
        persistence: PersistenceClass,
    ) -> ThreadCommit {
        let thread_id = ThreadId::new(thread_id).expect("thread id");
        let state = ThreadActorState {
            snapshot: AgentSnapshot {
                identity: AgentIdentity {
                    id: thread_id.clone(),
                    parent_id: None,
                    role: AgentRoleId::new("executor").expect("role"),
                    depth: 0,
                },
                state: AgentState::idle(),
                pending_inputs: 0,
                progress: None,
                last_turn: None,
                revision,
                event_sequence: revision,
                updated_at: i64::try_from(revision).expect("test revision fits i64"),
            },
            session: ThreadContextState::empty(),
            pending_inputs: VecDeque::new(),
            active_input: None,
        };
        ThreadCommit {
            agent_id: thread_id,
            persistence,
            expected_revision,
            facts: DurableCommitFacts::from_state(&state, Vec::new(), Vec::new(), None, None),
            next_state: state,
            mutation: ThreadMutation::SnapshotAndQueue,
        }
    }
}
