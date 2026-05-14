use std::future::Future;

use mai_protocol::{AgentId, TaskId, TaskStatus};

use crate::Result;
use crate::state::RuntimeState;

use super::{TaskUpdateOps, set_status, task, task_agents};

/// Supplies agent and persistence side effects for task lifecycle mutations.
pub(crate) trait TaskLifecycleOps: TaskUpdateOps {
    fn cancel_agent_for_task(
        &self,
        agent_id: AgentId,
        current_turn: Option<mai_protocol::TurnId>,
    ) -> impl Future<Output = Result<()>> + Send;

    fn delete_agent(&self, agent_id: AgentId) -> impl Future<Output = Result<()>> + Send;

    fn agent_current_turn(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<Option<mai_protocol::TurnId>>> + Send;

    fn delete_task_from_store(&self, task_id: TaskId) -> impl Future<Output = Result<()>> + Send;

    fn publish_task_deleted(&self, task_id: TaskId) -> impl Future<Output = ()> + Send;
}

pub(crate) async fn cancel_task(
    state: &RuntimeState,
    ops: &impl TaskLifecycleOps,
    task_id: TaskId,
) -> Result<()> {
    let task = task(state, task_id).await?;
    let agents = task_agents(state, task_id).await;
    for agent in agents {
        if let Ok(current_turn) = ops.agent_current_turn(agent.id).await {
            let _ = ops.cancel_agent_for_task(agent.id, current_turn).await;
        }
    }
    set_status(state, ops, &task, TaskStatus::Cancelled, None, None).await?;
    Ok(())
}

pub(crate) async fn delete_task(
    state: &RuntimeState,
    ops: &impl TaskLifecycleOps,
    task_id: TaskId,
) -> Result<()> {
    let _task = task(state, task_id).await?;
    let root_agents = task_agents(state, task_id)
        .await
        .into_iter()
        .filter(|agent| agent.parent_id.is_none())
        .map(|agent| agent.id)
        .collect::<Vec<_>>();
    for agent_id in root_agents {
        let _ = ops.delete_agent(agent_id).await;
    }
    ops.delete_task_from_store(task_id).await?;
    state.tasks.write().await.remove(&task_id);
    ops.publish_task_deleted(task_id).await;
    Ok(())
}
