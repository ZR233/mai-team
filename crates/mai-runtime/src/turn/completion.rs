use std::sync::Arc;

use mai_protocol::{AgentId, AgentStatus, ServiceEventKind, SessionId, TurnId, TurnStatus, now};
use mai_store::ConfigStore;
use serde_json::json;

use crate::events::RuntimeEvents;
use crate::state::AgentRecord;
use crate::turn::persistence::{self, AgentLogRecord};
use crate::{Result, RuntimeError};

pub(crate) struct TurnResult {
    pub(crate) turn_id: TurnId,
    pub(crate) status: TurnStatus,
    pub(crate) agent_status: AgentStatus,
    pub(crate) final_text: Option<String>,
    pub(crate) error: Option<String>,
}

pub(crate) async fn finish_turn(
    store: &ConfigStore,
    events: &RuntimeEvents,
    agent: &Arc<AgentRecord>,
    agent_id: AgentId,
    session_id: SessionId,
    result: TurnResult,
) -> Result<()> {
    let _ = session_id;
    complete_turn_if_current(store, events, agent, agent_id, result).await?;
    Ok(())
}

pub(crate) async fn complete_turn_if_current(
    store: &ConfigStore,
    events: &RuntimeEvents,
    agent: &Arc<AgentRecord>,
    agent_id: AgentId,
    result: TurnResult,
) -> Result<bool> {
    let turn_id = result.turn_id;
    let session_id = {
        let mut active_turn = agent.active_turn.lock().expect("active turn lock");
        let active_session_id = active_turn
            .as_ref()
            .filter(|turn| turn.turn_id == turn_id)
            .map(|turn| turn.session_id);
        if active_session_id.is_some() {
            *active_turn = None;
        }
        active_session_id
    };
    let session_id = match session_id {
        Some(session_id) => session_id,
        None => {
            let current_turn = agent.summary.read().await.current_turn;
            if current_turn != Some(turn_id) {
                return Ok(false);
            }
            // Legacy in-memory records may not have active_turn populated; keep the turn's selected session.
            agent
                .sessions
                .lock()
                .await
                .first()
                .map(|session| session.summary.id)
                .ok_or(RuntimeError::TurnNotFound { agent_id, turn_id })?
        }
    };
    {
        let mut summary = agent.summary.write().await;
        if summary.current_turn != Some(turn_id) {
            return Ok(false);
        }
        summary.status = result.agent_status.clone();
        summary.current_turn = None;
        summary.updated_at = now();
        if let Some(error) = result.error {
            summary.last_error = Some(error);
        }
    }
    {
        let mut sessions = agent.sessions.lock().await;
        if let Some(session) = sessions
            .iter_mut()
            .find(|session| session.summary.id == session_id)
        {
            session.last_turn_response = result.final_text;
        }
    }
    persist_agent(store, agent).await?;
    let turn_status = result.status.clone();
    events
        .publish(ServiceEventKind::TurnCompleted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
            status: turn_status.clone(),
        })
        .await;
    persistence::record_agent_log(
        store,
        AgentLogRecord {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            level: "info",
            category: "turn",
            message: "turn completed",
            details: json!({ "status": turn_status }),
        },
    )
    .await;
    events
        .publish(ServiceEventKind::AgentStatusChanged {
            agent_id,
            status: result.agent_status,
        })
        .await;
    Ok(true)
}

async fn persist_agent(store: &ConfigStore, agent: &AgentRecord) -> Result<()> {
    let summary = agent.summary.read().await.clone();
    store
        .save_agent(&summary, agent.system_prompt.as_deref())
        .await?;
    Ok(())
}
