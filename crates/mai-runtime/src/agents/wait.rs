use std::time::Duration;

use mai_protocol::{AgentId, AgentStatus, AgentSummary};
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

use super::{AgentServiceOps, is_agent_wait_complete};
use crate::{Result, RuntimeError};

pub(crate) async fn wait_agent(
    ops: &dyn AgentServiceOps,
    agent_id: AgentId,
    timeout: Duration,
) -> Result<AgentSummary> {
    wait_agent_with_cancel(ops, agent_id, timeout, &CancellationToken::new()).await
}

pub(crate) async fn wait_agent_with_cancel(
    ops: &dyn AgentServiceOps,
    agent_id: AgentId,
    timeout: Duration,
    cancellation_token: &CancellationToken,
) -> Result<AgentSummary> {
    let deadline = Instant::now() + timeout;
    loop {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        let agent = ops.agent(agent_id).await?;
        let summary = agent.summary.read().await.clone();
        if summary.current_turn.is_none()
            || matches!(
                summary.status,
                AgentStatus::Completed
                    | AgentStatus::Failed
                    | AgentStatus::Cancelled
                    | AgentStatus::Deleted
                    | AgentStatus::Idle
            )
        {
            return Ok(summary);
        }
        if Instant::now() >= deadline {
            return Ok(summary);
        }
        tokio::select! {
            _ = sleep(Duration::from_millis(250)) => {},
            _ = cancellation_token.cancelled() => return Err(RuntimeError::TurnCancelled),
        }
    }
}

pub(crate) async fn wait_agent_until_complete_with_cancel(
    ops: &dyn AgentServiceOps,
    agent_id: AgentId,
    cancellation_token: &CancellationToken,
) -> Result<AgentSummary> {
    loop {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        let agent = ops.agent(agent_id).await?;
        let summary = agent.summary.read().await.clone();
        if is_agent_wait_complete(&summary) {
            return Ok(summary);
        }
        tokio::select! {
            _ = sleep(Duration::from_millis(250)) => {},
            _ = cancellation_token.cancelled() => return Err(RuntimeError::TurnCancelled),
        }
    }
}
