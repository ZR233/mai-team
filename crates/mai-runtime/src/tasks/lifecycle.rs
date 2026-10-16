use std::future::Future;

use mai_protocol::{AgentId, TaskId, TaskStatus};

use crate::state::RuntimeState;
use crate::{Result, RuntimeError};

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
    // 明确授权的资源删除：Agent 资源没有回收干净时不允许删除任务记录，否则泄漏的容器、
    // sidecar 与工作区会失去唯一能找到它们的父记录。失败时任务记录保持原样，调用方可以用
    // 同一个 id 重试；已经删除过的 Agent 视为幂等完成，不算失败。
    let mut failures = Vec::new();
    let mut first_failure = None;
    for agent_id in root_agents {
        match ops.delete_agent(agent_id).await {
            Ok(()) | Err(RuntimeError::AgentNotFound(_)) => {}
            Err(error) => {
                tracing::error!(
                    task_id = %task_id,
                    agent_id = %agent_id,
                    "failed to delete task agent resources during task deletion: {error}"
                );
                failures.push(format!("agent `{agent_id}`: {error}"));
                if first_failure.is_none() {
                    first_failure = Some(RuntimeError::InvalidInput(format!(
                        "task `{task_id}` agent `{agent_id}` resource deletion failed: {error}"
                    )));
                }
            }
        }
    }
    if let Some(error) = first_failure {
        tracing::error!(
            task_id = %task_id,
            failures = %failures.join("; "),
            "task deletion stopped before removing the task record"
        );
        return Err(error);
    }
    ops.delete_task_from_store(task_id).await?;
    state.tasks.write().await.remove(&task_id);
    ops.publish_task_deleted(task_id).await;
    Ok(())
}
