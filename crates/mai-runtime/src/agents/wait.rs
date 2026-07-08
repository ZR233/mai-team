use std::time::Duration;

use mai_protocol::{AgentId, AgentSummary};
use tokio_util::sync::CancellationToken;

use super::{AgentServiceOps, agent_wait_snapshot};
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
    wait_agent_with_options(
        ops,
        agent_id,
        pl_core::AgentWaitLoopOptions::new(timeout),
        cancellation_token,
    )
    .await
}

pub(crate) async fn wait_agent_until_complete_with_cancel(
    ops: &dyn AgentServiceOps,
    agent_id: AgentId,
    cancellation_token: &CancellationToken,
) -> Result<AgentSummary> {
    wait_agent_with_options(
        ops,
        agent_id,
        pl_core::AgentWaitLoopOptions::until_complete(),
        cancellation_token,
    )
    .await
}

async fn wait_agent_with_options(
    ops: &dyn AgentServiceOps,
    agent_id: AgentId,
    options: pl_core::AgentWaitLoopOptions,
    cancellation_token: &CancellationToken,
) -> Result<AgentSummary> {
    let result = pl_core::wait_for_agent_completion(
        || async {
            let agent = ops.agent(agent_id).await?;
            let summary = agent.summary.read().await.clone();
            Ok::<_, RuntimeError>((agent_wait_snapshot(&summary), summary))
        },
        options,
        cancellation_token,
    )
    .await
    .map_err(|error| match error {
        pl_core::AgentWaitLoopError::Cancelled => RuntimeError::TurnCancelled,
        pl_core::AgentWaitLoopError::Read(error) => error,
    })?;
    Ok(result.value)
}
