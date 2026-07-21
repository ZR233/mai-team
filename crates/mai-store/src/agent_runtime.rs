use std::collections::BTreeMap;

mod projection;
mod state;

use projection::load_session_projections;
use state::{stored_state, unix_timestamp_rfc3339, usize_to_i64};

use crate::records::{
    AgentHistoryRecord, AgentMessageRecord, AgentPendingInputRecord, AgentRuntimeEventRecord,
    AgentRuntimeStateRecord, AgentRuntimeTraceRecord, AgentSessionRecord, AgentTurnRecord,
    SessionEventJournalRecord, SessionViewSnapshotRecord,
};
use crate::*;

/// 不依赖 pl-core 的 token usage 持久化值对象。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTokenUsage {
    pub prompt_tokens: u64,
    pub cached_prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

/// agent actor 最新 snapshot 的 serde 文档。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAgentRuntimeState {
    pub agent_id: String,
    pub parent_id: Option<String>,
    pub role: String,
    pub depth: u32,
    pub lifecycle: String,
    pub activity: String,
    pub active_turn_id: Option<String>,
    pub active_session_id: Option<String>,
    pub pending_inputs: usize,
    pub last_turn: Option<serde_json::Value>,
    pub revision: u64,
    pub event_sequence: u64,
    pub updated_at: i64,
}

/// canonical session、usage 和 UI message projection。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredAgentRuntimeSession {
    pub session_id: String,
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub history_items: Vec<serde_json::Value>,
    pub messages: Vec<AgentMessage>,
    pub usage: StoredTokenUsage,
    pub last_context_tokens: Option<u64>,
    pub trace_sequence: u64,
    pub session_event_sequence: u64,
}

/// 可恢复的 FIFO 输入。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredAgentPendingInput {
    pub turn_id: String,
    pub session_id: String,
    pub message: String,
    pub metadata: serde_json::Value,
    pub queued_at: i64,
}

/// turn durable record。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAgentTurn {
    pub turn_id: String,
    pub session_id: String,
    pub status: String,
    pub error: Option<String>,
    pub usage: StoredTokenUsage,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

/// 已随 snapshot transaction 提交的 framework event。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredAgentRuntimeEvent {
    pub sequence: u64,
    pub created_at: i64,
    pub payload: serde_json::Value,
}

/// 已随 turn transaction 提交的原始 trace event。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredAgentRuntimeTrace {
    pub sequence: u64,
    pub payload: serde_json::Value,
}

/// 不依赖 pl-protocol 的 canonical session event journal 条目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredSessionEvent {
    pub sequence: u64,
    pub emitted_at: i64,
    pub payload: serde_json::Value,
}

/// runtime 重启时重建 session hub 所需的 projection 与有界 journal。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredSessionProjection {
    pub session_id: String,
    pub through_sequence: u64,
    pub snapshot: serde_json::Value,
    pub durable_events: Vec<StoredSessionEvent>,
}

/// repository 恢复一个 actor 所需的完整文档。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredAgentRuntime {
    pub state: StoredAgentRuntimeState,
    pub sessions: Vec<StoredAgentRuntimeSession>,
    pub pending_inputs: Vec<StoredAgentPendingInput>,
    pub session_projections: Vec<StoredSessionProjection>,
}

/// mai-runtime 提交给 store 的原子 CAS 文档。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRuntimeCommitDocument {
    pub expected_revision: Option<u64>,
    pub mutation: StoredAgentRuntimeMutation,
    pub runtime: StoredAgentRuntime,
    pub turns: Vec<StoredAgentTurn>,
    pub events: Vec<StoredAgentRuntimeEvent>,
    pub traces: Vec<StoredAgentRuntimeTrace>,
    pub session_projection: Option<StoredSessionProjection>,
}

/// 与 PL repository mutation 对齐，但不让 mai-store 反向依赖 pl-core。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum StoredAgentRuntimeMutation {
    SnapshotAndQueue,
    ReplaceSession { session_id: String },
    AppendTrace,
    AppendSessionEvents { session_id: String },
}

/// CAS 提交结果；冲突不会写入任何记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRuntimeCommitOutcome {
    Applied,
    RevisionConflict { actual_revision: Option<u64> },
}

impl MaiStore {
    /// 加载全部 PL actor durable state，不读取产品内存状态。
    pub async fn load_agent_runtimes(&self) -> Result<Vec<StoredAgentRuntime>> {
        let mut db = self.db.clone();
        let mut state_rows = Query::<List<AgentRuntimeStateRecord>>::all()
            .exec(&mut db)
            .await?;
        state_rows.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
        let mut runtimes = Vec::with_capacity(state_rows.len());
        for row in state_rows {
            let agent_id = row.agent_id.clone();
            let sessions = load_sessions(&mut db, &agent_id).await?;
            let session_projections = load_session_projections(&mut db, &sessions).await?;
            runtimes.push(StoredAgentRuntime {
                state: stored_state(row)?,
                sessions,
                pending_inputs: load_pending_inputs(&mut db, &agent_id).await?,
                session_projections,
            });
        }
        Ok(runtimes)
    }

    /// 按 framework agent ID 加载单个 canonical actor 文档。
    pub async fn load_agent_runtime(&self, agent_id: &str) -> Result<Option<StoredAgentRuntime>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<AgentRuntimeStateRecord>>::filter(
            AgentRuntimeStateRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        let Some(row) = rows.pop() else {
            return Ok(None);
        };
        let sessions = load_sessions(&mut db, agent_id).await?;
        let session_projections = load_session_projections(&mut db, &sessions).await?;
        Ok(Some(StoredAgentRuntime {
            state: stored_state(row)?,
            sessions,
            pending_inputs: load_pending_inputs(&mut db, agent_id).await?,
            session_projections,
        }))
    }

    /// 原子执行 revision CAS 与 actor 全量状态、turn、event、trace 写入。
    pub async fn commit_agent_runtime(
        &self,
        document: AgentRuntimeCommitDocument,
    ) -> Result<AgentRuntimeCommitOutcome> {
        let agent_id = document.runtime.state.agent_id.clone();
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        let existing_states = Query::<List<AgentRuntimeStateRecord>>::filter(
            AgentRuntimeStateRecord::fields()
                .agent_id()
                .eq(agent_id.clone()),
        )
        .exec(&mut tx)
        .await?;
        let actual_revision = existing_states
            .first()
            .map(|state| i64_to_u64(state.revision));
        if actual_revision != document.expected_revision {
            return Ok(AgentRuntimeCommitOutcome::RevisionConflict { actual_revision });
        }

        let existing_sessions = Query::<List<AgentSessionRecord>>::filter(
            AgentSessionRecord::fields().agent_id().eq(agent_id.clone()),
        )
        .exec(&mut tx)
        .await?
        .into_iter()
        .map(|session| (session.id.clone(), session))
        .collect::<BTreeMap<_, _>>();

        replace_state(&mut tx, &document.runtime.state).await?;
        match &document.mutation {
            StoredAgentRuntimeMutation::SnapshotAndQueue => {
                replace_sessions(
                    &mut tx,
                    &agent_id,
                    &existing_sessions,
                    &document.runtime.sessions,
                )
                .await?;
                replace_pending_inputs(&mut tx, &agent_id, &document.runtime.pending_inputs)
                    .await?;
            }
            StoredAgentRuntimeMutation::ReplaceSession { session_id } => {
                let session = document
                    .runtime
                    .sessions
                    .iter()
                    .find(|session| session.session_id == *session_id)
                    .ok_or_else(|| {
                        StoreError::InvalidConfig(format!(
                            "replacement session `{session_id}` is missing from runtime commit"
                        ))
                    })?;
                replace_one_session(
                    &mut tx,
                    &agent_id,
                    existing_sessions.get(session_id),
                    session,
                )
                .await?;
            }
            StoredAgentRuntimeMutation::AppendTrace => {
                if let Some(session_id) = &document.runtime.state.active_session_id {
                    let session = document
                        .runtime
                        .sessions
                        .iter()
                        .find(|session| session.session_id == *session_id)
                        .ok_or_else(|| {
                            StoreError::InvalidConfig(format!(
                                "trace session `{session_id}` is missing from runtime commit"
                            ))
                        })?;
                    replace_session_record(
                        &mut tx,
                        &agent_id,
                        existing_sessions.get(session_id),
                        session,
                    )
                    .await?;
                }
            }
            StoredAgentRuntimeMutation::AppendSessionEvents { session_id } => {
                let session = document
                    .runtime
                    .sessions
                    .iter()
                    .find(|session| session.session_id == *session_id)
                    .ok_or_else(|| {
                        StoreError::InvalidConfig(format!(
                            "session event target `{session_id}` is missing from runtime commit"
                        ))
                    })?;
                replace_session_record(
                    &mut tx,
                    &agent_id,
                    existing_sessions.get(session_id),
                    session,
                )
                .await?;
            }
        }
        upsert_turns(&mut tx, &agent_id, &document.turns).await?;
        append_events(&mut tx, &agent_id, &document.events).await?;
        append_traces(&mut tx, &agent_id, &document.traces).await?;
        if let Some(projection) = &document.session_projection {
            replace_session_projection(&mut tx, projection).await?;
        }
        tx.commit().await?;
        Ok(AgentRuntimeCommitOutcome::Applied)
    }
}

async fn replace_state(
    tx: &mut toasty::Transaction<'_>,
    state: &StoredAgentRuntimeState,
) -> Result<()> {
    Query::<List<AgentRuntimeStateRecord>>::filter(
        AgentRuntimeStateRecord::fields()
            .agent_id()
            .eq(state.agent_id.clone()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    toasty::create!(AgentRuntimeStateRecord {
        agent_id: state.agent_id.clone(),
        parent_id: state.parent_id.clone(),
        role: state.role.clone(),
        depth: i64::from(state.depth),
        lifecycle: state.lifecycle.clone(),
        activity: state.activity.clone(),
        active_turn_id: state.active_turn_id.clone(),
        active_session_id: state.active_session_id.clone(),
        pending_inputs: usize_to_i64(state.pending_inputs),
        last_turn_json: state
            .last_turn
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?,
        revision: u64_to_i64(state.revision),
        event_sequence: u64_to_i64(state.event_sequence),
        updated_at: state.updated_at,
    })
    .exec(&mut *tx)
    .await?;
    Ok(())
}

async fn replace_sessions(
    tx: &mut toasty::Transaction<'_>,
    agent_id: &str,
    existing: &BTreeMap<String, AgentSessionRecord>,
    sessions: &[StoredAgentRuntimeSession],
) -> Result<()> {
    for delete in [Query::<List<AgentSessionRecord>>::filter(
        AgentSessionRecord::fields()
            .agent_id()
            .eq(agent_id.to_string()),
    )
    .delete()]
    {
        delete.exec(&mut *tx).await?;
    }
    Query::<List<AgentHistoryRecord>>::filter(
        AgentHistoryRecord::fields()
            .agent_id()
            .eq(agent_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<AgentMessageRecord>>::filter(
        AgentMessageRecord::fields()
            .agent_id()
            .eq(agent_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;

    for session in sessions {
        insert_session_record(tx, agent_id, existing.get(&session.session_id), session).await?;
        insert_session_content(tx, agent_id, session).await?;
    }
    Ok(())
}

async fn replace_one_session(
    tx: &mut toasty::Transaction<'_>,
    agent_id: &str,
    prior: Option<&AgentSessionRecord>,
    session: &StoredAgentRuntimeSession,
) -> Result<()> {
    Query::<List<AgentSessionRecord>>::filter(
        AgentSessionRecord::fields()
            .id()
            .eq(session.session_id.clone()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<AgentHistoryRecord>>::filter(
        AgentHistoryRecord::fields()
            .session_id()
            .eq(session.session_id.clone()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<AgentMessageRecord>>::filter(
        AgentMessageRecord::fields()
            .session_id()
            .eq(session.session_id.clone()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    insert_session_record(tx, agent_id, prior, session).await?;
    insert_session_content(tx, agent_id, session).await
}

async fn replace_session_record(
    tx: &mut toasty::Transaction<'_>,
    agent_id: &str,
    prior: Option<&AgentSessionRecord>,
    session: &StoredAgentRuntimeSession,
) -> Result<()> {
    Query::<List<AgentSessionRecord>>::filter(
        AgentSessionRecord::fields()
            .id()
            .eq(session.session_id.clone()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    insert_session_record(tx, agent_id, prior, session).await
}

async fn insert_session_record(
    tx: &mut toasty::Transaction<'_>,
    agent_id: &str,
    prior: Option<&AgentSessionRecord>,
    session: &StoredAgentRuntimeSession,
) -> Result<()> {
    toasty::create!(AgentSessionRecord {
        id: session.session_id.clone(),
        agent_id: agent_id.to_string(),
        title: session
            .title
            .clone()
            .or_else(|| prior.map(|value| value.title.clone()))
            .unwrap_or_else(|| "New session".to_string()),
        created_at: prior
            .map(|value| value.created_at.clone())
            .unwrap_or_else(|| unix_timestamp_rfc3339(session.created_at)),
        updated_at: unix_timestamp_rfc3339(session.updated_at),
        input_tokens: u64_to_i64(session.usage.prompt_tokens),
        cached_input_tokens: u64_to_i64(session.usage.cached_prompt_tokens),
        output_tokens: u64_to_i64(session.usage.completion_tokens),
        reasoning_output_tokens: u64_to_i64(session.usage.reasoning_tokens),
        total_tokens: u64_to_i64(session.usage.total_tokens),
        last_context_tokens: session.last_context_tokens.map(u64_to_i64),
        trace_sequence: u64_to_i64(session.trace_sequence),
        session_event_sequence: u64_to_i64(session.session_event_sequence),
    })
    .exec(&mut *tx)
    .await?;
    Ok(())
}

async fn insert_session_content(
    tx: &mut toasty::Transaction<'_>,
    agent_id: &str,
    session: &StoredAgentRuntimeSession,
) -> Result<()> {
    for (position, item) in session.history_items.iter().enumerate() {
        toasty::create!(AgentHistoryRecord {
            id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            session_id: session.session_id.clone(),
            position: usize_to_i64(position),
            item_json: serde_json::to_string(item)?,
        })
        .exec(&mut *tx)
        .await?;
    }
    for (position, message) in session.messages.iter().enumerate() {
        toasty::create!(AgentMessageRecord {
            id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            session_id: session.session_id.clone(),
            position: usize_to_i64(position),
            role: message.role.to_string(),
            content: message.content.clone(),
            created_at: message.created_at.to_rfc3339(),
        })
        .exec(&mut *tx)
        .await?;
    }
    Ok(())
}

async fn replace_pending_inputs(
    tx: &mut toasty::Transaction<'_>,
    agent_id: &str,
    inputs: &[StoredAgentPendingInput],
) -> Result<()> {
    Query::<List<AgentPendingInputRecord>>::filter(
        AgentPendingInputRecord::fields()
            .agent_id()
            .eq(agent_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    for (position, input) in inputs.iter().enumerate() {
        toasty::create!(AgentPendingInputRecord {
            id: format!("{agent_id}:{}", input.turn_id),
            agent_id: agent_id.to_string(),
            position: usize_to_i64(position),
            turn_id: input.turn_id.clone(),
            session_id: input.session_id.clone(),
            message: input.message.clone(),
            metadata_json: serde_json::to_string(&input.metadata)?,
            queued_at: input.queued_at,
        })
        .exec(&mut *tx)
        .await?;
    }
    Ok(())
}

async fn upsert_turns(
    tx: &mut toasty::Transaction<'_>,
    agent_id: &str,
    turns: &[StoredAgentTurn],
) -> Result<()> {
    for turn in turns {
        Query::<List<AgentTurnRecord>>::filter(
            AgentTurnRecord::fields().turn_id().eq(turn.turn_id.clone()),
        )
        .delete()
        .exec(&mut *tx)
        .await?;
        toasty::create!(AgentTurnRecord {
            turn_id: turn.turn_id.clone(),
            agent_id: agent_id.to_string(),
            session_id: turn.session_id.clone(),
            status: turn.status.clone(),
            error: turn.error.clone(),
            prompt_tokens: u64_to_i64(turn.usage.prompt_tokens),
            cached_prompt_tokens: u64_to_i64(turn.usage.cached_prompt_tokens),
            completion_tokens: u64_to_i64(turn.usage.completion_tokens),
            reasoning_tokens: u64_to_i64(turn.usage.reasoning_tokens),
            total_tokens: u64_to_i64(turn.usage.total_tokens),
            started_at: turn.started_at,
            finished_at: turn.finished_at,
        })
        .exec(&mut *tx)
        .await?;
    }
    Ok(())
}

async fn append_events(
    tx: &mut toasty::Transaction<'_>,
    agent_id: &str,
    events: &[StoredAgentRuntimeEvent],
) -> Result<()> {
    for event in events {
        toasty::create!(AgentRuntimeEventRecord {
            id: format!("{agent_id}:{}", event.sequence),
            agent_id: agent_id.to_string(),
            sequence: u64_to_i64(event.sequence),
            created_at: event.created_at,
            event_json: serde_json::to_string(&event.payload)?,
        })
        .exec(&mut *tx)
        .await?;
    }
    Ok(())
}

async fn append_traces(
    tx: &mut toasty::Transaction<'_>,
    agent_id: &str,
    traces: &[StoredAgentRuntimeTrace],
) -> Result<()> {
    for trace in traces {
        toasty::create!(AgentRuntimeTraceRecord {
            id: format!("{agent_id}:{}:{}", trace.sequence, Uuid::new_v4()),
            agent_id: agent_id.to_string(),
            sequence: u64_to_i64(trace.sequence),
            trace_json: serde_json::to_string(&trace.payload)?,
        })
        .exec(&mut *tx)
        .await?;
    }
    Ok(())
}

async fn replace_session_projection(
    tx: &mut toasty::Transaction<'_>,
    projection: &StoredSessionProjection,
) -> Result<()> {
    Query::<List<SessionViewSnapshotRecord>>::filter(
        SessionViewSnapshotRecord::fields()
            .session_id()
            .eq(projection.session_id.clone()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    toasty::create!(SessionViewSnapshotRecord {
        session_id: projection.session_id.clone(),
        through_sequence: u64_to_i64(projection.through_sequence),
        snapshot_json: serde_json::to_string(&projection.snapshot)?,
        updated_at: projection
            .durable_events
            .last()
            .map_or(0, |event| event.emitted_at),
    })
    .exec(&mut *tx)
    .await?;

    for event in &projection.durable_events {
        let id = format!("{}:{}", projection.session_id, event.sequence);
        Query::<List<SessionEventJournalRecord>>::filter(
            SessionEventJournalRecord::fields().id().eq(id.clone()),
        )
        .delete()
        .exec(&mut *tx)
        .await?;
        toasty::create!(SessionEventJournalRecord {
            id,
            session_id: projection.session_id.clone(),
            sequence: u64_to_i64(event.sequence),
            emitted_at: event.emitted_at,
            event_json: serde_json::to_string(&event.payload)?,
        })
        .exec(&mut *tx)
        .await?;
    }

    let mut journal = Query::<List<SessionEventJournalRecord>>::filter(
        SessionEventJournalRecord::fields()
            .session_id()
            .eq(projection.session_id.clone()),
    )
    .exec(&mut *tx)
    .await?;
    journal.sort_by_key(|event| event.sequence);
    let stale_count = journal.len().saturating_sub(4096);
    for stale in journal.into_iter().take(stale_count) {
        Query::<List<SessionEventJournalRecord>>::filter(
            SessionEventJournalRecord::fields().id().eq(stale.id),
        )
        .delete()
        .exec(&mut *tx)
        .await?;
    }
    Ok(())
}

async fn load_sessions(db: &mut Db, agent_id: &str) -> Result<Vec<StoredAgentRuntimeSession>> {
    let mut rows = Query::<List<AgentSessionRecord>>::filter(
        AgentSessionRecord::fields()
            .agent_id()
            .eq(agent_id.to_string()),
    )
    .exec(db)
    .await?;
    rows.sort_by(|left, right| left.id.cmp(&right.id));
    let mut sessions = Vec::with_capacity(rows.len());
    for row in rows {
        let mut history = Query::<List<AgentHistoryRecord>>::filter(
            AgentHistoryRecord::fields().session_id().eq(row.id.clone()),
        )
        .exec(db)
        .await?;
        history.retain(|item| item.agent_id == agent_id);
        history.sort_by_key(|item| item.position);
        let mut messages = Query::<List<AgentMessageRecord>>::filter(
            AgentMessageRecord::fields().session_id().eq(row.id.clone()),
        )
        .exec(db)
        .await?;
        messages.retain(|item| item.agent_id == agent_id);
        messages.sort_by_key(|item| item.position);
        sessions.push(StoredAgentRuntimeSession {
            session_id: row.id,
            title: Some(row.title),
            created_at: parse_utc(&row.created_at)?.timestamp(),
            updated_at: parse_utc(&row.updated_at)?.timestamp(),
            history_items: history
                .into_iter()
                .map(|item| serde_json::from_str(&item.item_json).map_err(Into::into))
                .collect::<Result<_>>()?,
            messages: messages
                .into_iter()
                .map(AgentMessageRecord::into_message)
                .collect::<Result<_>>()?,
            usage: StoredTokenUsage {
                prompt_tokens: i64_to_u64(row.input_tokens),
                cached_prompt_tokens: i64_to_u64(row.cached_input_tokens),
                completion_tokens: i64_to_u64(row.output_tokens),
                reasoning_tokens: i64_to_u64(row.reasoning_output_tokens),
                total_tokens: i64_to_u64(row.total_tokens),
            },
            last_context_tokens: row.last_context_tokens.map(i64_to_u64),
            trace_sequence: i64_to_u64(row.trace_sequence),
            session_event_sequence: i64_to_u64(row.session_event_sequence),
        });
    }
    Ok(sessions)
}

async fn load_pending_inputs(db: &mut Db, agent_id: &str) -> Result<Vec<StoredAgentPendingInput>> {
    let mut rows = Query::<List<AgentPendingInputRecord>>::filter(
        AgentPendingInputRecord::fields()
            .agent_id()
            .eq(agent_id.to_string()),
    )
    .exec(db)
    .await?;
    rows.sort_by_key(|row| row.position);
    rows.into_iter()
        .map(|row| {
            Ok(StoredAgentPendingInput {
                turn_id: row.turn_id,
                session_id: row.session_id,
                message: row.message,
                metadata: serde_json::from_str(&row.metadata_json)?,
                queued_at: row.queued_at,
            })
        })
        .collect()
}
