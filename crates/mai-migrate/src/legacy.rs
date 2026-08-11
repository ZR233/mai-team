use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result, bail};
use pl_model::TokenUsage;
use pl_protocol::{
    AgentSessionSnapshot, AgentWorkingState, ModelContextItem, PinnedContextSection, SessionNote,
    ThreadContextDisposition, ThreadTurnHistory, Turn, TurnState,
};
use rusqlite::Transaction;
use serde_json::Value;

use crate::MigrationReport;
use crate::archive::{self, ContextRow};

mod actor;
mod source;

use actor::actor_runtime;
use source::{
    ensure_quiescent, load_agents, load_context, load_runtimes, load_sessions, load_turns,
    validate_messages,
};

#[derive(Debug)]
pub(crate) struct ConvertedData {
    pub runtimes: Vec<RuntimeRow>,
    pub histories: Vec<ThreadTurnHistory>,
    pub review_histories: Vec<(String, ThreadTurnHistory)>,
    pub agents: usize,
}

#[derive(Debug)]
pub(crate) struct RuntimeRow {
    pub thread_id: String,
    pub revision: u64,
    pub document_json: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
struct LegacyAgent {
    id: String,
    parent_id: Option<String>,
    role: Option<String>,
    updated_at: String,
}

#[derive(Debug, Clone)]
struct LegacyRuntime {
    active_session_id: Option<String>,
    lifecycle: String,
    activity: String,
    active_turn_id: Option<String>,
    pending_inputs: i64,
    last_turn_json: Option<String>,
    revision: i64,
    event_sequence: i64,
    updated_at: i64,
}

#[derive(Debug, Clone)]
struct LegacySession {
    id: String,
    agent_id: String,
    created_at: String,
    updated_at: String,
    usage: TokenUsage,
    last_context_tokens: Option<u64>,
    trace_sequence: u64,
}

#[derive(Debug, Clone)]
struct LegacyTurn {
    id: String,
    agent_id: String,
    session_id: String,
    status: String,
    error: Option<String>,
    started_at: Option<i64>,
    finished_at: Option<i64>,
}

pub(crate) fn validate_convertible(transaction: &Transaction<'_>) -> Result<MigrationReport> {
    let converted = convert(transaction)?;
    Ok(MigrationReport {
        source_schema: "27".to_string(),
        target_schema: "28".to_string(),
        already_current: false,
        agents: converted.agents,
        canonical_threads: converted.runtimes.len(),
        turns: converted.histories.len(),
        items: converted
            .histories
            .iter()
            .map(|history| history.items.len())
            .sum(),
        archived_review_runs: converted.review_histories.len(),
    })
}

pub(crate) fn convert(transaction: &Transaction<'_>) -> Result<ConvertedData> {
    ensure_quiescent(transaction)?;
    let agents = load_agents(transaction)?;
    let runtimes = load_runtimes(transaction)?;
    let sessions = load_sessions(transaction)?;
    let context = load_context(transaction, &sessions)?;
    validate_messages(transaction, &sessions)?;
    let turns = load_turns(transaction, &sessions)?;
    let depths = agent_depths(&agents)?;

    let mut sessions_by_agent = BTreeMap::<String, Vec<LegacySession>>::new();
    for session in sessions.values().cloned() {
        sessions_by_agent
            .entry(session.agent_id.clone())
            .or_default()
            .push(session);
    }
    let mut turns_by_session = BTreeMap::<String, Vec<LegacyTurn>>::new();
    for turn in turns {
        turns_by_session
            .entry(turn.session_id.clone())
            .or_default()
            .push(turn);
    }

    let mut canonical = HashMap::<String, Option<String>>::new();
    for agent in &agents {
        let selected = select_canonical_session(
            agent,
            sessions_by_agent.get_mut(&agent.id).map(Vec::as_mut_slice),
            runtimes.get(&agent.id),
        )?;
        canonical.insert(agent.id.clone(), selected);
    }

    let mut runtime_rows = Vec::with_capacity(agents.len());
    let mut histories = Vec::new();
    for agent in &agents {
        let selected = canonical.get(&agent.id).cloned().flatten();
        let selected_session = selected.as_ref().and_then(|id| sessions.get(id));
        runtime_rows.push(actor_runtime(
            agent,
            depths[&agent.id],
            runtimes.get(&agent.id),
            selected_session,
            selected_session
                .and_then(|session| context.get(&session.id))
                .map(Vec::as_slice)
                .unwrap_or_default(),
        )?);

        for session in sessions_by_agent
            .get(&agent.id)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let disposition = if selected.as_deref() == Some(session.id.as_str()) {
                ThreadContextDisposition::Active
            } else {
                ThreadContextDisposition::RolledBack
            };
            if let Some(history) = archive::session_history(
                &agent.id,
                &format!("migrated-session:{}", session.id),
                parse_time(&session.created_at)?,
                parse_time(&session.updated_at)?,
                disposition,
                context
                    .get(&session.id)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            )? {
                histories.push(history);
            }
            for turn in turns_by_session
                .get(&session.id)
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                histories.push(legacy_turn_history(turn, disposition)?);
            }
        }
    }

    let review_histories = load_review_histories(transaction)?;
    validate_unique_ids(&runtime_rows, &histories)?;
    Ok(ConvertedData {
        runtimes: runtime_rows,
        histories,
        review_histories,
        agents: agents.len(),
    })
}

fn select_canonical_session(
    agent: &LegacyAgent,
    sessions: Option<&mut [LegacySession]>,
    runtime: Option<&LegacyRuntime>,
) -> Result<Option<String>> {
    let Some(sessions) = sessions else {
        if runtime
            .and_then(|runtime| runtime.active_session_id.as_ref())
            .is_some()
        {
            bail!("Agent {} 的 active session 不存在", agent.id);
        }
        return Ok(None);
    };
    if let Some(active) = runtime.and_then(|runtime| runtime.active_session_id.as_ref()) {
        if sessions.iter().any(|session| &session.id == active) {
            return Ok(Some(active.clone()));
        }
        bail!("Agent {} 的 active session {active} 不存在", agent.id);
    }
    sessions.sort_by(|left, right| {
        left.updated_at
            .cmp(&right.updated_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(sessions.last().map(|session| session.id.clone()))
}

fn validate_context_value(value: &Value) -> Result<()> {
    match value.get("type").and_then(Value::as_str) {
        Some("message" | "toolResult" | "compaction" | "responses") => {
            serde_json::from_value::<ModelContextItem>(value.clone())?;
        }
        Some("pinnedContext") => {
            serde_json::from_value::<PinnedContextSection>(
                value
                    .get("section")
                    .cloned()
                    .context("pinnedContext 缺少 section")?,
            )?;
        }
        Some("sessionNote") => {
            serde_json::from_value::<SessionNote>(
                value
                    .get("note")
                    .cloned()
                    .context("sessionNote 缺少 note")?,
            )?;
        }
        Some(other) => bail!("未知 context item 类型 `{other}`"),
        None => bail!("context item 缺少 type"),
    }
    Ok(())
}

fn session_snapshot(context: &[ContextRow]) -> Result<AgentSessionSnapshot> {
    let mut transcript = Vec::new();
    let mut working_state = AgentWorkingState::default();
    for row in context {
        match row.value.get("type").and_then(Value::as_str) {
            Some("message" | "toolResult" | "compaction" | "responses") => {
                transcript.push(serde_json::from_value(row.value.clone())?);
            }
            Some("pinnedContext") => working_state.sections.push(serde_json::from_value(
                row.value
                    .get("section")
                    .cloned()
                    .context("pinnedContext 缺少 section")?,
            )?),
            Some("sessionNote") => {
                let note = serde_json::from_value::<SessionNote>(
                    row.value
                        .get("note")
                        .cloned()
                        .context("sessionNote 缺少 note")?,
                )?;
                if working_state.session_note.replace(note).is_some() {
                    bail!("同一 session 存在多个 sessionNote");
                }
            }
            Some(other) => bail!("未知 context item 类型 `{other}`"),
            None => bail!("context item 缺少 type"),
        }
    }
    working_state
        .sections
        .sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
    working_state.revision = working_state
        .sections
        .iter()
        .map(|section| section.revision)
        .chain(working_state.session_note.iter().map(|note| note.revision))
        .max()
        .unwrap_or(0);
    Ok(AgentSessionSnapshot {
        transcript,
        working_state,
    })
}

fn legacy_turn_history(
    turn: &LegacyTurn,
    disposition: ThreadContextDisposition,
) -> Result<ThreadTurnHistory> {
    let updated_at = turn.finished_at.or(turn.started_at).unwrap_or_default();
    let state = match turn.status.as_str() {
        "completed" => TurnState::Completed,
        "failed" => TurnState::Failed {
            reason: turn
                .error
                .clone()
                .unwrap_or_else(|| "历史 Turn 失败".to_string()),
        },
        "cancelled" | "interrupted" => TurnState::Interrupted {
            reason: turn
                .error
                .clone()
                .unwrap_or_else(|| "历史 Turn 已中断".to_string()),
        },
        other => bail!("未知历史 Turn 状态: {other}"),
    };
    Ok(ThreadTurnHistory {
        turn: Turn {
            id: turn.id.clone(),
            thread_id: turn.agent_id.clone(),
            state,
            failure: None,
            started_at: turn.started_at,
            updated_at,
            completed_at: turn.finished_at,
        },
        items: Vec::new(),
        context_disposition: disposition,
    })
}

fn agent_depths(agents: &[LegacyAgent]) -> Result<HashMap<String, u32>> {
    let parents = agents
        .iter()
        .map(|agent| (agent.id.clone(), agent.parent_id.clone()))
        .collect::<HashMap<_, _>>();
    let mut depths = HashMap::with_capacity(agents.len());
    for agent in agents {
        let mut visiting = BTreeSet::new();
        resolve_agent_depth(&agent.id, &parents, &mut depths, &mut visiting)?;
    }
    Ok(depths)
}

fn resolve_agent_depth(
    agent_id: &str,
    parents: &HashMap<String, Option<String>>,
    depths: &mut HashMap<String, u32>,
    visiting: &mut BTreeSet<String>,
) -> Result<u32> {
    if let Some(depth) = depths.get(agent_id) {
        return Ok(*depth);
    }
    if !visiting.insert(agent_id.to_string()) {
        bail!("Agent parent 关系存在环: {agent_id}");
    }
    let parent = parents
        .get(agent_id)
        .with_context(|| format!("Agent {agent_id} 不存在"))?;
    let depth = match parent {
        Some(parent_id) => resolve_agent_depth(parent_id, parents, depths, visiting)?
            .checked_add(1)
            .context("Agent depth 溢出")?,
        None => 0,
    };
    visiting.remove(agent_id);
    depths.insert(agent_id.to_string(), depth);
    Ok(depth)
}

fn load_review_histories(
    transaction: &Transaction<'_>,
) -> Result<Vec<(String, ThreadTurnHistory)>> {
    let mut statement = transaction.prepare(
        "SELECT id, reviewer_agent_id, turn_id, status, started_at, finished_at,
                messages_json, events_json FROM project_review_runs ORDER BY id",
    )?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?
        .map(|row| {
            let (id, reviewer, turn, status, started, finished, messages, events) = row?;
            let history = archive::review_history(&archive::ReviewArchiveSource {
                run_id: id.clone(),
                reviewer_thread_id: reviewer,
                requested_turn_id: turn,
                status,
                started_at: parse_time(&started)?,
                finished_at: finished.as_deref().map(parse_time).transpose()?,
                messages_json: messages,
                events_json: events,
            })?;
            Ok((id, history))
        })
        .collect()
}

fn validate_unique_ids(runtimes: &[RuntimeRow], histories: &[ThreadTurnHistory]) -> Result<()> {
    let mut threads = BTreeSet::new();
    for runtime in runtimes {
        if !threads.insert(runtime.thread_id.as_str()) {
            bail!("重复 canonical Thread id {}", runtime.thread_id);
        }
    }
    let mut turns = BTreeSet::new();
    let mut items = BTreeSet::new();
    for history in histories {
        if !turns.insert(history.turn.id.as_str()) {
            bail!("重复 Turn id {}", history.turn.id);
        }
        for item in &history.items {
            if item.thread_id != history.turn.thread_id || item.turn_id != history.turn.id {
                bail!("Item {} 的 Thread/Turn 归属不一致", item.id);
            }
            if !items.insert(item.id.as_str()) {
                bail!("重复 Thread Item id {}", item.id);
            }
        }
    }
    Ok(())
}

fn parse_time(value: &str) -> Result<i64> {
    Ok(chrono::DateTime::parse_from_rfc3339(value)?.timestamp())
}

fn nonnegative(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

fn optional_nonnegative(value: Option<i64>) -> rusqlite::Result<Option<u64>> {
    value.map(nonnegative).transpose()
}
