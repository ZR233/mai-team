use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use pl_core::{
    ActiveKind, AgentActivityState, AgentIdentity, AgentLifecycleState, AgentRoleId, AgentSnapshot,
    AgentTurnOutcome, ThreadId, TurnOutcomeKind,
};
use pl_model::TokenUsage;
use pl_protocol::AgentSessionSnapshot;
use serde::Serialize;
use serde_json::Value;

use super::{
    ContextRow, LegacyAgent, LegacyRuntime, LegacySession, RuntimeRow, nonnegative, parse_time,
    session_snapshot,
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ActorDocument {
    snapshot: AgentSnapshot,
    context: ActorContext,
    pending_inputs: Vec<pl_core::DurableMailboxEnvelope>,
    active_input: Option<pl_core::DurableMailboxEnvelope>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ActorContext {
    metadata: Value,
    session: AgentSessionSnapshot,
    usage: TokenUsage,
    billing_by_turn: BTreeMap<String, pl_protocol::TurnBillingRecord>,
    last_context_tokens: Option<u64>,
    trace_sequence: u64,
    thread_revision: u64,
}

pub(super) fn actor_runtime(
    agent: &LegacyAgent,
    depth: u32,
    runtime: Option<&LegacyRuntime>,
    session: Option<&LegacySession>,
    context: &[ContextRow],
) -> Result<RuntimeRow> {
    let session_snapshot = session_snapshot(context)?;
    let revision = runtime
        .map(|runtime| nonnegative(runtime.revision))
        .transpose()?
        .unwrap_or(0);
    let updated_at = runtime
        .map(|runtime| runtime.updated_at)
        .unwrap_or(parse_time(&agent.updated_at)?);
    let document = ActorDocument {
        snapshot: AgentSnapshot {
            identity: AgentIdentity {
                id: ThreadId::new(agent.id.clone())?,
                parent_id: agent.parent_id.clone().map(ThreadId::new).transpose()?,
                role: AgentRoleId::new(
                    agent
                        .role
                        .clone()
                        .unwrap_or_else(|| "assistant".to_string()),
                )?,
                depth,
            },
            lifecycle: parse_lifecycle(runtime.map(|runtime| runtime.lifecycle.as_str()))?,
            activity: parse_activity(runtime.map(|runtime| runtime.activity.as_str()))?,
            active_turn_id: runtime
                .and_then(|runtime| runtime.active_turn_id.clone())
                .map(pl_core::TurnId::new)
                .transpose()?,
            pending_inputs: runtime
                .map(|runtime| usize::try_from(runtime.pending_inputs))
                .transpose()?
                .unwrap_or(0),
            progress: None,
            last_turn: runtime
                .and_then(|runtime| runtime.last_turn_json.as_deref())
                .map(|raw| legacy_last_turn(raw, &agent.id))
                .transpose()?,
            revision,
            event_sequence: runtime
                .map(|runtime| nonnegative(runtime.event_sequence))
                .transpose()?
                .unwrap_or(0),
            updated_at,
        },
        context: ActorContext {
            metadata: Value::Null,
            session: session_snapshot,
            usage: session
                .map(|session| session.usage.clone())
                .unwrap_or_default(),
            billing_by_turn: BTreeMap::new(),
            last_context_tokens: session.and_then(|session| session.last_context_tokens),
            trace_sequence: session.map(|session| session.trace_sequence).unwrap_or(0),
            thread_revision: 0,
        },
        pending_inputs: Vec::new(),
        active_input: None,
    };
    Ok(RuntimeRow {
        thread_id: agent.id.clone(),
        revision,
        document_json: serde_json::to_string(&document)?,
        updated_at,
    })
}

fn legacy_last_turn(raw: &str, thread_id: &str) -> Result<AgentTurnOutcome> {
    let value = serde_json::from_str::<Value>(raw)?;
    let kind = match value
        .get("kind")
        .and_then(Value::as_str)
        .context("last turn 缺少 kind")?
    {
        "completed" => TurnOutcomeKind::Completed,
        "cancelled" => TurnOutcomeKind::Cancelled,
        "failed" => TurnOutcomeKind::Failed,
        "budget_limited" | "budgetLimited" => TurnOutcomeKind::BudgetLimited,
        other => bail!("未知 last turn kind `{other}`"),
    };
    Ok(AgentTurnOutcome {
        turn_id: pl_core::TurnId::new(
            value
                .get("turnId")
                .and_then(Value::as_str)
                .context("last turn 缺少 turnId")?,
        )?,
        thread_id: pl_core::ThreadId::new(thread_id)?,
        kind,
        reason: value
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        failure: None,
        budget_limit: None,
        rollover_compacted: false,
        rollover_compaction_error: None,
        usage: value
            .get("usage")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default(),
        finished_at: value
            .get("finishedAt")
            .and_then(Value::as_i64)
            .context("last turn 缺少 finishedAt")?,
    })
}

fn parse_lifecycle(value: Option<&str>) -> Result<AgentLifecycleState> {
    Ok(match value.unwrap_or("active") {
        "active" => AgentLifecycleState::Active,
        "closing" => AgentLifecycleState::Closing,
        "closed" => AgentLifecycleState::Closed,
        "faulted" => AgentLifecycleState::Faulted,
        other => bail!("未知 Agent lifecycle `{other}`"),
    })
}

fn parse_activity(value: Option<&str>) -> Result<AgentActivityState> {
    Ok(match value.unwrap_or("idle") {
        "idle" => AgentActivityState::Idle,
        "queued" => AgentActivityState::Queued,
        "running" => AgentActivityState::Active(ActiveKind::Running),
        "waiting_tool" | "waitingTool" => AgentActivityState::Active(ActiveKind::WaitingTool),
        "waiting_interaction" | "waitingInteraction" => {
            AgentActivityState::Active(ActiveKind::WaitingInteraction)
        }
        "cancelling" => AgentActivityState::Cancelling,
        other => bail!("未知 Agent activity `{other}`"),
    })
}
