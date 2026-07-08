use std::time::Duration;

use mai_protocol::{AgentId, AgentStatus, SessionId, TurnId};
use pl_core::{
    AgentInputBusyAction, AgentInputInitialAction, AgentInputSubmission, AgentInputTurnMode,
};

use super::{AgentInputOps, AgentServiceOps, prepare_turn, wait_agent};
use crate::state::{AgentRecord, QueuedAgentInput};
use crate::{Result, RuntimeError};

pub(crate) struct SendInputRequest {
    pub(crate) target: AgentId,
    pub(crate) session_id: Option<SessionId>,
    pub(crate) message: String,
    pub(crate) skill_mentions: Vec<String>,
    pub(crate) mode: AgentInputTurnMode,
    pub(crate) cancel_grace: Duration,
}

pub(crate) async fn send_input_to_agent(
    service: &dyn AgentServiceOps,
    input_ops: &impl AgentInputOps,
    request: SendInputRequest,
) -> Result<AgentInputSubmission> {
    let agent = service.agent(request.target).await?;
    let mode = request.mode;
    match mode.initial_action() {
        AgentInputInitialAction::Queue => return Ok(queue_agent_input(&agent, request).await),
        AgentInputInitialAction::StartTurn => {}
        AgentInputInitialAction::InterruptThenStart => {
            let current_turn = agent.summary.read().await.current_turn;
            if let Some(turn_id) = current_turn {
                input_ops.cancel_agent_turn(request.target, turn_id).await?;
            } else {
                agent
                    .cancel_requested
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                input_ops
                    .set_agent_status(&agent, AgentStatus::Cancelled, None)
                    .await?;
            }
            wait_agent(service, request.target, request.cancel_grace).await?;
        }
    }
    match prepare_turn(service, request.target).await {
        Ok((agent, turn_id)) => {
            let session_id = service
                .resolve_session_id(request.target, request.session_id)
                .await?;
            input_ops.spawn_turn(
                &agent,
                request.target,
                session_id,
                turn_id,
                request.message,
                request.skill_mentions,
            );
            Ok(AgentInputSubmission::started(turn_id.to_string()))
        }
        Err(RuntimeError::AgentBusy(_))
            if matches!(mode.busy_action(), AgentInputBusyAction::Queue) =>
        {
            Ok(queue_agent_input(&agent, request).await)
        }
        Err(err) => Err(err),
    }
}

async fn queue_agent_input(agent: &AgentRecord, request: SendInputRequest) -> AgentInputSubmission {
    agent.pending_inputs.lock().await.push(QueuedAgentInput {
        session_id: request.session_id,
        message: request.message,
        skill_mentions: request.skill_mentions,
    });
    AgentInputSubmission::queued()
}

pub(crate) async fn start_next_queued_input(
    service: &dyn AgentServiceOps,
    input_ops: &impl AgentInputOps,
    agent_id: AgentId,
) -> Result<Option<TurnId>> {
    let agent = service.agent(agent_id).await?;
    let Some(attempt) = agent.pending_inputs.lock().await.take_start_attempt() else {
        return Ok(None);
    };
    let session_id = service
        .resolve_session_id(agent_id, attempt.input().session_id)
        .await?;
    let (agent, turn_id) = match prepare_turn(service, agent_id).await {
        Ok(turn) => turn,
        Err(RuntimeError::AgentBusy(_)) => {
            agent
                .pending_inputs
                .lock()
                .await
                .restore_start_attempt(attempt);
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    let input = attempt.into_input();
    input_ops.spawn_turn(
        &agent,
        agent_id,
        session_id,
        turn_id,
        input.message,
        input.skill_mentions,
    );
    Ok(Some(turn_id))
}

pub(crate) async fn start_next_queued_input_after_turn(
    service: &dyn AgentServiceOps,
    input_ops: &impl AgentInputOps,
    agent_id: AgentId,
) {
    if let Err(err) = start_next_queued_input(service, input_ops, agent_id).await {
        tracing::warn!("failed to start queued agent input: {err}");
    }
}
