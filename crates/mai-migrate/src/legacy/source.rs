use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result, bail};
use pl_model::TokenUsage;
use rusqlite::Transaction;
use serde_json::Value;

use super::{
    LegacyAgent, LegacyRuntime, LegacySession, LegacyTurn, nonnegative, optional_nonnegative,
    validate_context_value,
};
use crate::archive::ContextRow;

pub(super) fn ensure_quiescent(transaction: &Transaction<'_>) -> Result<()> {
    let pending: i64 =
        transaction.query_row("SELECT COUNT(*) FROM agent_pending_inputs", [], |row| {
            row.get(0)
        })?;
    if pending != 0 {
        bail!("存在 {pending} 条未消费 mailbox 输入，拒绝离线迁移");
    }
    let active: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM agent_runtime_states
         WHERE active_turn_id IS NOT NULL OR pending_inputs != 0 OR activity != 'idle'",
        [],
        |row| row.get(0),
    )?;
    if active != 0 {
        bail!("存在 {active} 个非静止 Agent runtime，拒绝离线迁移");
    }
    let jobs: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM project_review_jobs
         WHERE status NOT IN ('succeeded', 'skipped', 'failed', 'superseded', 'cancelled')
            OR lease_owner IS NOT NULL OR lease_expires_at IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    if jobs != 0 {
        bail!("存在 {jobs} 个非终态 review job 或 lease，拒绝离线迁移");
    }
    let runs: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM project_review_runs
         WHERE status NOT IN (
            'completed', 'failed', 'succeeded', 'retryable_failed',
            'permanent_failed', 'interrupted', 'cancelled'
         )",
        [],
        |row| row.get(0),
    )?;
    if runs != 0 {
        bail!("存在 {runs} 个非终态 review run，拒绝离线迁移");
    }
    Ok(())
}

pub(super) fn load_agents(transaction: &Transaction<'_>) -> Result<Vec<LegacyAgent>> {
    let mut statement = transaction.prepare(
        "SELECT id, runtime_agent_id, parent_id, role, updated_at
         FROM agents ORDER BY id",
    )?;
    statement
        .query_map([], |row| {
            let id = row.get::<_, String>(0)?;
            let runtime_id = row.get::<_, Option<String>>(1)?;
            if runtime_id
                .as_deref()
                .is_some_and(|runtime_id| runtime_id != id)
            {
                return Err(rusqlite::Error::InvalidQuery);
            }
            Ok(LegacyAgent {
                id,
                parent_id: row.get(2)?,
                role: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("agents.runtime_agent_id 必须为空或等于产品 Agent id")
}

pub(super) fn load_runtimes(
    transaction: &Transaction<'_>,
) -> Result<HashMap<String, LegacyRuntime>> {
    let mut statement = transaction.prepare(
        "SELECT agent_id, active_session_id, lifecycle, activity, active_turn_id,
                pending_inputs, last_turn_json, revision, event_sequence, updated_at
         FROM agent_runtime_states",
    )?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                LegacyRuntime {
                    active_session_id: row.get(1)?,
                    lifecycle: row.get(2)?,
                    activity: row.get(3)?,
                    active_turn_id: row.get(4)?,
                    pending_inputs: row.get(5)?,
                    last_turn_json: row.get(6)?,
                    revision: row.get(7)?,
                    event_sequence: row.get(8)?,
                    updated_at: row.get(9)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()
        .map_err(Into::into)
}

pub(super) fn load_sessions(
    transaction: &Transaction<'_>,
) -> Result<BTreeMap<String, LegacySession>> {
    let mut statement = transaction.prepare(
        "SELECT id, agent_id, created_at, updated_at, input_tokens, cached_input_tokens,
                output_tokens, reasoning_output_tokens, total_tokens, last_context_tokens,
                trace_sequence FROM agent_sessions ORDER BY agent_id, updated_at, id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(LegacySession {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            created_at: row.get(2)?,
            updated_at: row.get(3)?,
            usage: TokenUsage {
                prompt_tokens: nonnegative(row.get(4)?)?,
                cached_prompt_tokens: nonnegative(row.get(5)?)?,
                completion_tokens: nonnegative(row.get(6)?)?,
                reasoning_tokens: nonnegative(row.get(7)?)?,
                total_tokens: nonnegative(row.get(8)?)?,
                cache_write_tokens: 0,
            },
            last_context_tokens: optional_nonnegative(row.get(9)?)?,
            trace_sequence: nonnegative(row.get(10)?)?,
        })
    })?;
    let mut sessions = BTreeMap::new();
    for session in rows {
        let session = session?;
        if sessions.insert(session.id.clone(), session).is_some() {
            bail!("重复 session id");
        }
    }
    Ok(sessions)
}

pub(super) fn load_context(
    transaction: &Transaction<'_>,
    sessions: &BTreeMap<String, LegacySession>,
) -> Result<BTreeMap<String, Vec<ContextRow>>> {
    let mut statement = transaction.prepare(
        "SELECT id, agent_id, session_id, position, item_json
         FROM agent_history_items ORDER BY session_id, position, id",
    )?;
    let mut context = BTreeMap::<String, Vec<ContextRow>>::new();
    let mut positions = BTreeSet::<(String, i64)>::new();
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
        ))
    })? {
        let (id, agent_id, session_id, position, raw) = row?;
        let session = sessions
            .get(&session_id)
            .with_context(|| format!("history item {id} 引用不存在的 session {session_id}"))?;
        if session.agent_id != agent_id {
            bail!("history item {id} 的 agent/session 归属不一致");
        }
        if !positions.insert((session_id.clone(), position)) {
            bail!("session {session_id} 存在重复 history position {position}");
        }
        let value = serde_json::from_str::<Value>(&raw)
            .with_context(|| format!("history item {id} JSON 非法"))?;
        validate_context_value(&value)
            .with_context(|| format!("history item {id} 不是有效 PL context state"))?;
        context.entry(session_id).or_default().push(ContextRow {
            id,
            position,
            value,
        });
    }
    for rows in context.values_mut() {
        rows.sort_by_key(|row| row.position);
    }
    Ok(context)
}

pub(super) fn validate_messages(
    transaction: &Transaction<'_>,
    sessions: &BTreeMap<String, LegacySession>,
) -> Result<()> {
    let mut statement = transaction.prepare(
        "SELECT id, agent_id, session_id, position FROM agent_messages
         ORDER BY session_id, position, id",
    )?;
    let mut positions = BTreeSet::<(String, i64)>::new();
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })? {
        let (id, agent_id, session_id, position) = row?;
        let session = sessions
            .get(&session_id)
            .with_context(|| format!("message {id} 引用不存在的 session {session_id}"))?;
        if session.agent_id != agent_id {
            bail!("message {id} 的 agent/session 归属不一致");
        }
        if !positions.insert((session_id.clone(), position)) {
            bail!("session {session_id} 存在重复 message position {position}");
        }
    }
    Ok(())
}

pub(super) fn load_turns(
    transaction: &Transaction<'_>,
    sessions: &BTreeMap<String, LegacySession>,
) -> Result<Vec<LegacyTurn>> {
    let mut statement = transaction.prepare(
        "SELECT turn_id, agent_id, session_id, status, error, started_at, finished_at
         FROM agent_turns ORDER BY COALESCE(started_at, finished_at, 0), turn_id",
    )?;
    let mut turns = Vec::new();
    for row in statement.query_map([], |row| {
        Ok(LegacyTurn {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            status: row.get(3)?,
            error: row.get(4)?,
            started_at: row.get(5)?,
            finished_at: row.get(6)?,
        })
    })? {
        let turn = row?;
        let session = sessions
            .get(&turn.session_id)
            .with_context(|| format!("turn {} 引用不存在的 session", turn.id))?;
        if session.agent_id != turn.agent_id {
            bail!("turn {} 的 agent/session 归属不一致", turn.id);
        }
        turns.push(turn);
    }
    Ok(turns)
}
