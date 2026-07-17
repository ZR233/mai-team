use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use mai_protocol::{AgentMessage, MessageRole};
use mai_store::{
    AgentRuntimeCommitDocument, AgentRuntimeCommitOutcome as StoreCommitOutcome, MaiStore,
    StoredAgentPendingInput, StoredAgentRuntime, StoredAgentRuntimeEvent,
    StoredAgentRuntimeSession, StoredAgentRuntimeState, StoredAgentRuntimeTrace, StoredAgentTurn,
    StoredTokenUsage,
};
use pl_core::{
    AgentActivityState, AgentCommit, AgentCommitOutcome, AgentDurableState, AgentId, AgentIdentity,
    AgentLifecycleState, AgentSession, AgentSessionState, AgentSnapshot, AgentStateRepository,
    AgentTurnOutcome, PendingAgentInput, RestoredAgentRuntime, SessionId, TurnId, TurnOutcomeKind,
};
use pl_protocol::{Message as ModelMessage, MessageRole as ModelMessageRole, ModelContextItem};

use crate::{Result, RuntimeError};

/// 使用 mai-store transaction 实现的 PL canonical state repository。
#[derive(Clone)]
pub(crate) struct MaiAgentRepository {
    store: Arc<MaiStore>,
}

impl MaiAgentRepository {
    pub(crate) fn new(store: Arc<MaiStore>) -> Self {
        Self { store }
    }
}

impl AgentStateRepository for MaiAgentRepository {
    type Error = RuntimeError;

    async fn restore_runtime(&self) -> Result<Vec<RestoredAgentRuntime>> {
        self.store
            .load_agent_runtimes()
            .await?
            .into_iter()
            .map(runtime_from_store)
            .collect()
    }

    async fn commit(&self, commit: AgentCommit) -> Result<AgentCommitOutcome> {
        let document = commit_to_store(commit)?;
        match self.store.commit_agent_runtime(document).await? {
            StoreCommitOutcome::Applied => Ok(AgentCommitOutcome::Applied),
            StoreCommitOutcome::RevisionConflict { actual_revision } => {
                Ok(AgentCommitOutcome::RevisionConflict { actual_revision })
            }
        }
    }
}

fn commit_to_store(commit: AgentCommit) -> Result<AgentRuntimeCommitDocument> {
    let AgentCommit {
        expected_revision,
        next_state,
        events,
        trace_events,
        ..
    } = commit;
    let timestamp = datetime_from_unix(next_state.snapshot.updated_at);
    let sessions = next_state
        .sessions
        .values()
        .map(|session| session_to_store(session, timestamp))
        .collect::<Result<Vec<_>>>()?;
    let pending_inputs = next_state
        .pending_inputs
        .iter()
        .map(input_to_store)
        .collect();
    let turns = turns_from_state(&next_state);
    let runtime_events = events
        .into_iter()
        .map(|event| {
            Ok(StoredAgentRuntimeEvent {
                sequence: event.sequence,
                created_at: event.created_at,
                payload: serde_json::to_value(event).map_err(json_error)?,
            })
        })
        .collect::<Result<_>>()?;
    let traces = trace_events
        .into_iter()
        .map(|trace| {
            Ok(StoredAgentRuntimeTrace {
                sequence: trace.sequence,
                payload: serde_json::to_value(trace).map_err(json_error)?,
            })
        })
        .collect::<Result<_>>()?;
    Ok(AgentRuntimeCommitDocument {
        expected_revision,
        runtime: StoredAgentRuntime {
            state: snapshot_to_store(&next_state.snapshot)?,
            sessions,
            pending_inputs,
        },
        turns,
        events: runtime_events,
        traces,
    })
}

fn runtime_from_store(runtime: StoredAgentRuntime) -> Result<RestoredAgentRuntime> {
    let sessions = runtime
        .sessions
        .into_iter()
        .map(|session| {
            let id = SessionId::new(session.session_id).map_err(RuntimeError::Model)?;
            let items = session
                .history_items
                .into_iter()
                .map(|item| serde_json::from_value(item).map_err(json_error))
                .collect::<Result<Vec<ModelContextItem>>>()?;
            Ok((
                id.clone(),
                AgentSessionState {
                    id,
                    metadata: serde_json::json!({
                        "title": session.title.unwrap_or_else(|| "New session".to_string()),
                        "createdAt": session.created_at,
                        "updatedAt": session.updated_at,
                    }),
                    session: AgentSession::from_items(items),
                    usage: usage_from_store(session.usage),
                    last_context_tokens: session.last_context_tokens,
                    trace_sequence: session.trace_sequence,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let pending_inputs = runtime
        .pending_inputs
        .into_iter()
        .map(input_from_store)
        .collect::<Result<VecDeque<_>>>()?;
    Ok(RestoredAgentRuntime {
        state: AgentDurableState {
            snapshot: snapshot_from_store(runtime.state)?,
            sessions,
            pending_inputs,
        },
    })
}

fn snapshot_to_store(snapshot: &AgentSnapshot) -> Result<StoredAgentRuntimeState> {
    Ok(StoredAgentRuntimeState {
        agent_id: snapshot.identity.id.to_string(),
        parent_id: snapshot
            .identity
            .parent_id
            .as_ref()
            .map(ToString::to_string),
        role: snapshot.identity.role.to_string(),
        depth: snapshot.identity.depth,
        lifecycle: lifecycle_name(snapshot.lifecycle).to_string(),
        activity: activity_name(snapshot.activity).to_string(),
        active_turn_id: snapshot.active_turn_id.as_ref().map(ToString::to_string),
        active_session_id: snapshot.active_session_id.as_ref().map(ToString::to_string),
        pending_inputs: snapshot.pending_inputs,
        last_turn: snapshot
            .last_turn
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(json_error)?,
        revision: snapshot.revision,
        event_sequence: snapshot.event_sequence,
        updated_at: snapshot.updated_at,
    })
}

fn snapshot_from_store(state: StoredAgentRuntimeState) -> Result<AgentSnapshot> {
    Ok(AgentSnapshot {
        identity: AgentIdentity {
            id: AgentId::new(state.agent_id).map_err(RuntimeError::Model)?,
            parent_id: state
                .parent_id
                .map(AgentId::new)
                .transpose()
                .map_err(RuntimeError::Model)?,
            role: pl_core::AgentRoleId::new(state.role).map_err(RuntimeError::Model)?,
            depth: state.depth,
        },
        lifecycle: parse_lifecycle(&state.lifecycle)?,
        activity: parse_activity(&state.activity)?,
        active_turn_id: state
            .active_turn_id
            .map(TurnId::new)
            .transpose()
            .map_err(RuntimeError::Model)?,
        active_session_id: state
            .active_session_id
            .map(SessionId::new)
            .transpose()
            .map_err(RuntimeError::Model)?,
        pending_inputs: state.pending_inputs,
        last_turn: state
            .last_turn
            .map(|value| serde_json::from_value(value).map_err(json_error))
            .transpose()?,
        revision: state.revision,
        event_sequence: state.event_sequence,
        updated_at: state.updated_at,
    })
}

fn session_to_store(
    session: &AgentSessionState,
    timestamp: DateTime<Utc>,
) -> Result<StoredAgentRuntimeSession> {
    Ok(StoredAgentRuntimeSession {
        session_id: session.id.to_string(),
        title: session
            .metadata
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        created_at: session
            .metadata
            .get("createdAt")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or_else(|| timestamp.timestamp()),
        updated_at: timestamp.timestamp(),
        history_items: session
            .session
            .items()
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(json_error)?,
        messages: session
            .session
            .messages()
            .iter()
            .map(|message| project_message(message, timestamp))
            .collect(),
        usage: usage_to_store(&session.usage),
        last_context_tokens: session.last_context_tokens,
        trace_sequence: session.trace_sequence,
    })
}

fn project_message(message: &ModelMessage, created_at: DateTime<Utc>) -> AgentMessage {
    AgentMessage {
        role: match message.role {
            ModelMessageRole::System => MessageRole::System,
            ModelMessageRole::User => MessageRole::User,
            ModelMessageRole::Assistant => MessageRole::Assistant,
            ModelMessageRole::Tool => MessageRole::Tool,
        },
        content: pl_core::message_content_text(&message.content),
        created_at,
    }
}

fn turns_from_state(state: &AgentDurableState) -> Vec<StoredAgentTurn> {
    let mut turns = BTreeMap::new();
    for input in &state.pending_inputs {
        turns.insert(
            input.turn_id.clone(),
            StoredAgentTurn {
                turn_id: input.turn_id.to_string(),
                session_id: input.session_id.to_string(),
                status: "queued".to_string(),
                error: None,
                usage: StoredTokenUsage::default(),
                started_at: None,
                finished_at: None,
            },
        );
    }
    if let (Some(turn_id), Some(session_id)) = (
        state.snapshot.active_turn_id.clone(),
        state.snapshot.active_session_id.clone(),
    ) {
        turns.insert(
            turn_id.clone(),
            StoredAgentTurn {
                turn_id: turn_id.to_string(),
                session_id: session_id.to_string(),
                status: activity_name(state.snapshot.activity).to_string(),
                error: None,
                usage: StoredTokenUsage::default(),
                started_at: Some(state.snapshot.updated_at),
                finished_at: None,
            },
        );
    }
    if let Some(outcome) = &state.snapshot.last_turn {
        turns.insert(outcome.turn_id.clone(), outcome_to_store(outcome));
    }
    turns.into_values().collect()
}

fn outcome_to_store(outcome: &AgentTurnOutcome) -> StoredAgentTurn {
    StoredAgentTurn {
        turn_id: outcome.turn_id.to_string(),
        session_id: outcome.session_id.to_string(),
        status: outcome_name(outcome.kind).to_string(),
        error: outcome.reason.clone(),
        usage: usage_to_store(&outcome.usage),
        started_at: None,
        finished_at: Some(outcome.finished_at),
    }
}

fn input_to_store(input: &PendingAgentInput) -> StoredAgentPendingInput {
    StoredAgentPendingInput {
        turn_id: input.turn_id.to_string(),
        session_id: input.session_id.to_string(),
        message: input.message.clone(),
        metadata: input.metadata.clone(),
        queued_at: input.queued_at,
    }
}

fn input_from_store(input: StoredAgentPendingInput) -> Result<PendingAgentInput> {
    Ok(PendingAgentInput {
        turn_id: TurnId::new(input.turn_id).map_err(RuntimeError::Model)?,
        session_id: SessionId::new(input.session_id).map_err(RuntimeError::Model)?,
        message: input.message,
        metadata: input.metadata,
        queued_at: input.queued_at,
    })
}

fn usage_to_store(usage: &pl_model::TokenUsage) -> StoredTokenUsage {
    StoredTokenUsage {
        prompt_tokens: usage.prompt_tokens,
        cached_prompt_tokens: usage.cached_prompt_tokens,
        completion_tokens: usage.completion_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        total_tokens: usage.total_tokens,
    }
}

fn usage_from_store(usage: StoredTokenUsage) -> pl_model::TokenUsage {
    pl_model::TokenUsage {
        prompt_tokens: usage.prompt_tokens,
        cached_prompt_tokens: usage.cached_prompt_tokens,
        completion_tokens: usage.completion_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        total_tokens: usage.total_tokens,
    }
}

fn lifecycle_name(state: AgentLifecycleState) -> &'static str {
    match state {
        AgentLifecycleState::Active => "active",
        AgentLifecycleState::Closing => "closing",
        AgentLifecycleState::Closed => "closed",
        AgentLifecycleState::Faulted => "faulted",
    }
}

fn parse_lifecycle(value: &str) -> Result<AgentLifecycleState> {
    match value {
        "active" => Ok(AgentLifecycleState::Active),
        "closing" => Ok(AgentLifecycleState::Closing),
        "closed" => Ok(AgentLifecycleState::Closed),
        "faulted" => Ok(AgentLifecycleState::Faulted),
        value => Err(RuntimeError::InvalidInput(format!(
            "unknown agent lifecycle `{value}`"
        ))),
    }
}

fn activity_name(state: AgentActivityState) -> &'static str {
    match state {
        AgentActivityState::Idle => "idle",
        AgentActivityState::Queued => "queued",
        AgentActivityState::Running => "running",
        AgentActivityState::WaitingTool => "waiting_tool",
        AgentActivityState::WaitingInteraction => "waiting_interaction",
    }
}

fn parse_activity(value: &str) -> Result<AgentActivityState> {
    match value {
        "idle" => Ok(AgentActivityState::Idle),
        "queued" => Ok(AgentActivityState::Queued),
        "running" => Ok(AgentActivityState::Running),
        "waiting_tool" => Ok(AgentActivityState::WaitingTool),
        "waiting_interaction" => Ok(AgentActivityState::WaitingInteraction),
        value => Err(RuntimeError::InvalidInput(format!(
            "unknown agent activity `{value}`"
        ))),
    }
}

fn outcome_name(kind: TurnOutcomeKind) -> &'static str {
    match kind {
        TurnOutcomeKind::Completed => "completed",
        TurnOutcomeKind::Cancelled => "cancelled",
        TurnOutcomeKind::Failed => "failed",
        TurnOutcomeKind::BudgetLimited => "budget_limited",
    }
}

fn datetime_from_unix(timestamp: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(timestamp, 0).unwrap_or_else(Utc::now)
}

fn json_error(error: serde_json::Error) -> RuntimeError {
    RuntimeError::InvalidInput(format!("invalid agent runtime document: {error}"))
}
