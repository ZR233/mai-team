use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::future::{AbortHandle, Abortable};
use mai_protocol::{AgentId, AgentStatus, ServiceEventKind, SessionId, TurnId, TurnStatus, now};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::AgentServiceOps;
use crate::state::{AgentRecord, TurnControl};
use crate::turn::completion::TurnResult;
use crate::{Result, RuntimeError};

/// Runs the asynchronous body of an agent turn after the shared turn state and
/// cancellation handles have been installed on the agent record.
pub(crate) trait AgentTurnTaskOps: Send + Sync {
    fn run_turn_task(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
        cancellation_token: CancellationToken,
    ) -> impl Future<Output = ()> + Send + 'static;
}

/// Supplies the turn-control operations needed to interrupt, queue, and start
/// conversational input for an agent without exposing the full runtime.
pub(crate) trait AgentInputOps: Send + Sync {
    fn cancel_agent_turn(
        &self,
        agent_id: AgentId,
        turn_id: TurnId,
    ) -> impl Future<Output = Result<()>> + Send;

    fn set_agent_status(
        &self,
        agent: &Arc<AgentRecord>,
        status: AgentStatus,
        error: Option<String>,
    ) -> impl Future<Output = Result<()>> + Send;

    fn spawn_turn(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
    );
}

/// Provides the status, completion, and queue side effects needed to cancel an
/// agent or one of its active turns.
pub(crate) trait AgentCancelOps: Send + Sync {
    fn agent(&self, agent_id: AgentId) -> impl Future<Output = Result<Arc<AgentRecord>>> + Send;

    fn set_agent_status(
        &self,
        agent: &Arc<AgentRecord>,
        status: AgentStatus,
        error: Option<String>,
    ) -> impl Future<Output = Result<()>> + Send;

    fn complete_turn_if_current(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        result: TurnResult,
    ) -> impl Future<Output = Result<bool>> + Send;

    fn start_next_queued_input_after_turn(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = ()> + Send;

    fn turn_cancel_grace(&self) -> std::time::Duration;
}

pub(crate) async fn send_message(
    service: &dyn AgentServiceOps,
    turn_ops: &impl AgentTurnTaskOps,
    agent_id: AgentId,
    session_id: Option<SessionId>,
    message: String,
    skill_mentions: Vec<String>,
) -> Result<TurnId> {
    let session_id = service.resolve_session_id(agent_id, session_id).await?;
    let (agent, turn_id) = prepare_turn(service, agent_id).await?;
    spawn_turn(
        turn_ops,
        &agent,
        agent_id,
        session_id,
        turn_id,
        message,
        skill_mentions,
    );
    Ok(turn_id)
}

pub(crate) async fn start_agent_turn(
    service: &dyn AgentServiceOps,
    turn_ops: &impl AgentTurnTaskOps,
    agent_id: AgentId,
    message: String,
    skill_mentions: Vec<String>,
) -> Result<TurnId> {
    send_message(service, turn_ops, agent_id, None, message, skill_mentions).await
}

pub(crate) async fn prepare_turn(
    ops: &dyn AgentServiceOps,
    agent_id: AgentId,
) -> Result<(Arc<AgentRecord>, TurnId)> {
    let agent = ops.agent(agent_id).await?;
    let turn_id = Uuid::new_v4();
    let should_start = {
        let mut summary = agent.summary.write().await;
        if !summary.status.can_start_turn() {
            false
        } else {
            summary.status = AgentStatus::RunningTurn;
            summary.current_turn = Some(turn_id);
            summary.updated_at = now();
            summary.last_error = None;
            agent.cancel_requested.store(false, Ordering::SeqCst);
            true
        }
    };
    if !should_start {
        return Err(RuntimeError::AgentBusy(agent_id));
    }
    ops.persist_agent(&agent).await?;
    ops.publish(ServiceEventKind::AgentStatusChanged {
        agent_id,
        status: AgentStatus::RunningTurn,
    })
    .await;
    Ok((agent, turn_id))
}

pub(crate) fn spawn_turn(
    ops: &impl AgentTurnTaskOps,
    agent: &Arc<AgentRecord>,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    message: String,
    skill_mentions: Vec<String>,
) {
    let cancellation_token = CancellationToken::new();
    let task_token = cancellation_token.clone();
    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    let control = TurnControl {
        turn_id,
        session_id,
        cancellation_token,
        abort_handle: Some(abort_handle),
    };
    *agent.active_turn.lock().expect("active turn lock") = Some(control);
    tokio::spawn(Abortable::new(
        ops.run_turn_task(
            agent_id,
            session_id,
            turn_id,
            message,
            skill_mentions,
            task_token,
        ),
        abort_registration,
    ));
}

pub(crate) async fn cancel_agent(ops: &impl AgentCancelOps, agent_id: AgentId) -> Result<()> {
    let agent = ops.agent(agent_id).await?;
    let turn_id = agent.summary.read().await.current_turn;
    match turn_id {
        Some(turn_id) => cancel_agent_turn(ops, agent_id, turn_id).await,
        None => {
            agent.cancel_requested.store(true, Ordering::SeqCst);
            ops.set_agent_status(&agent, AgentStatus::Cancelled, None)
                .await
        }
    }
}

pub(crate) async fn cancel_agent_turn(
    ops: &impl AgentCancelOps,
    agent_id: AgentId,
    turn_id: TurnId,
) -> Result<()> {
    let agent = ops.agent(agent_id).await?;
    let control = agent.active_turn.lock().expect("active turn lock").clone();
    let current_turn = agent.summary.read().await.current_turn;
    if current_turn != Some(turn_id) && control.as_ref().map(|turn| turn.turn_id) != Some(turn_id) {
        return Ok(());
    }
    agent.cancel_requested.store(true, Ordering::SeqCst);
    if let Some(control) = control.filter(|turn| turn.turn_id == turn_id) {
        control.cancellation_token.cancel();
        if let Some(abort_handle) = control.abort_handle {
            let token = control.cancellation_token.clone();
            let cancel_grace = ops.turn_cancel_grace();
            tokio::spawn(async move {
                tokio::time::sleep(cancel_grace).await;
                if token.is_cancelled() {
                    abort_handle.abort();
                }
            });
        }
    }
    let completed = ops
        .complete_turn_if_current(
            &agent,
            agent_id,
            TurnResult {
                turn_id,
                status: TurnStatus::Cancelled,
                agent_status: AgentStatus::Cancelled,
                final_text: None,
                error: None,
            },
        )
        .await?;
    if completed {
        ops.start_next_queued_input_after_turn(agent_id).await;
    }
    Ok(())
}
