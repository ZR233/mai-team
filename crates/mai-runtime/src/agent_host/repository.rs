use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use mai_store::{
    MaiStore, StoredThreadRuntime, StoredThreadRuntimeEvent, StoredThreadTraceEvent,
    ThreadRuntimeCommitDocument, ThreadRuntimeCommitOutcome as StoreCommitOutcome,
    ThreadRuntimeTurnCommit,
};
use pl_core::{
    AgentSession, AgentSnapshot, DurableMailboxEnvelope, RestoredAgentRuntime,
    RestoredThreadSnapshot, ThreadActorState, ThreadCommit, ThreadCommitOutcome,
    ThreadContextState, ThreadRepository,
};
use pl_model::TokenUsage;
use pl_protocol::{AgentSessionSnapshot, TurnBillingRecord};
use serde::{Deserialize, Serialize};

use crate::{Result, RuntimeError};

/// 使用 mai-store transaction 实现的 PL canonical Thread repository。
#[derive(Clone)]
pub(crate) struct MaiAgentRepository {
    store: Arc<MaiStore>,
}

impl MaiAgentRepository {
    pub(crate) fn new(store: Arc<MaiStore>) -> Self {
        Self { store }
    }
}

impl ThreadRepository for MaiAgentRepository {
    type Error = RuntimeError;

    async fn restore_runtime(&self) -> Result<Vec<RestoredAgentRuntime>> {
        self.store
            .load_thread_runtimes()
            .await?
            .into_iter()
            .map(runtime_from_store)
            .collect()
    }

    async fn commit(&self, commit: ThreadCommit) -> Result<ThreadCommitOutcome> {
        let document = commit_to_store(commit)?;
        match self.store.commit_thread_runtime(document).await? {
            StoreCommitOutcome::Applied => Ok(ThreadCommitOutcome::Applied),
            StoreCommitOutcome::RevisionConflict { actual_revision } => {
                Ok(ThreadCommitOutcome::RevisionConflict { actual_revision })
            }
        }
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
    metadata: serde_json::Value,
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
