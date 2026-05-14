use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use mai_protocol::{AgentId, AgentRole, AgentStatus, AgentSummary, ProjectId};

use crate::state::AgentRecord;
use crate::{Result, RuntimeError};

/// Groups the status mutation needed while deleting an agent.
pub(crate) struct AgentDeleteStatusChange {
    pub(crate) status: AgentStatus,
    pub(crate) error: Option<String>,
}

/// Describes the container cleanup request for a deleted agent.
pub(crate) struct AgentContainerDeleteRequest {
    pub(crate) agent_id: AgentId,
    pub(crate) preferred_container_id: Option<String>,
}

/// Provides the narrow runtime side effects required to delete an agent tree.
pub(crate) trait AgentDeleteOps: Send + Sync {
    fn agent(&self, agent_id: AgentId) -> impl Future<Output = Result<Arc<AgentRecord>>> + Send;

    fn agent_summaries(&self) -> impl Future<Output = Vec<AgentSummary>> + Send;

    fn set_agent_status(
        &self,
        agent: Arc<AgentRecord>,
        change: AgentDeleteStatusChange,
    ) -> impl Future<Output = Result<()>> + Send;

    fn delete_agent_containers(
        &self,
        request: AgentContainerDeleteRequest,
    ) -> impl Future<Output = Result<Vec<String>>> + Send;

    fn cleanup_project_review_worktree(
        &self,
        project_id: ProjectId,
        reviewer_id: AgentId,
    ) -> impl Future<Output = Result<()>> + Send;

    fn delete_agent_from_store(&self, agent_id: AgentId)
    -> impl Future<Output = Result<()>> + Send;

    fn remove_agent_from_memory(&self, agent_id: AgentId) -> impl Future<Output = ()> + Send;

    fn publish_agent_deleted(&self, agent_id: AgentId) -> impl Future<Output = ()> + Send;
}

pub(crate) async fn delete_agent(ops: &impl AgentDeleteOps, agent_id: AgentId) -> Result<()> {
    let targets = descendant_delete_order(ops, agent_id).await?;
    for target_id in targets {
        delete_agent_record(ops, target_id).await?;
    }
    Ok(())
}

async fn delete_agent_record(ops: &impl AgentDeleteOps, agent_id: AgentId) -> Result<()> {
    let agent = ops.agent(agent_id).await?;
    let reviewer_project_id = {
        let summary = agent.summary.read().await;
        (summary.role == Some(AgentRole::Reviewer))
            .then_some(summary.project_id)
            .flatten()
    };
    agent.cancel_requested.store(true, Ordering::SeqCst);
    ops.set_agent_status(
        Arc::clone(&agent),
        AgentDeleteStatusChange {
            status: AgentStatus::DeletingContainer,
            error: None,
        },
    )
    .await?;
    if let Some(control) = agent.active_turn.lock().expect("active turn lock").clone() {
        control.cancellation_token.cancel();
        if let Some(abort_handle) = control.abort_handle {
            abort_handle.abort();
        }
    }
    if let Some(manager) = agent.mcp.write().await.take() {
        manager.shutdown().await;
    }
    let in_memory_container_id = agent
        .container
        .write()
        .await
        .take()
        .map(|container| container.id);
    let persisted_container_id = agent.summary.read().await.container_id.clone();
    let preferred_container_id = in_memory_container_id.or(persisted_container_id);
    let deleted = ops
        .delete_agent_containers(AgentContainerDeleteRequest {
            agent_id,
            preferred_container_id,
        })
        .await?;
    if !deleted.is_empty() {
        tracing::info!(
            agent_id = %agent_id,
            count = deleted.len(),
            "removed agent containers"
        );
    }
    if let Some(project_id) = reviewer_project_id
        && let Err(err) = ops
            .cleanup_project_review_worktree(project_id, agent_id)
            .await
    {
        tracing::warn!(
            project_id = %project_id,
            reviewer_id = %agent_id,
            "failed to clean project reviewer worktree during agent deletion: {err}"
        );
    }
    let _turn_guard = agent.turn_lock.lock().await;
    ops.set_agent_status(
        Arc::clone(&agent),
        AgentDeleteStatusChange {
            status: AgentStatus::Deleted,
            error: None,
        },
    )
    .await?;
    ops.delete_agent_from_store(agent_id).await?;
    ops.remove_agent_from_memory(agent_id).await;
    ops.publish_agent_deleted(agent_id).await;
    Ok(())
}

async fn descendant_delete_order(
    ops: &impl AgentDeleteOps,
    root_id: AgentId,
) -> Result<Vec<AgentId>> {
    let summaries = ops.agent_summaries().await;
    if !summaries.iter().any(|summary| summary.id == root_id) {
        return Err(RuntimeError::AgentNotFound(root_id));
    }

    Ok(descendant_delete_order_from_summaries(root_id, &summaries))
}

pub(crate) fn descendant_delete_order_from_summaries(
    root_id: AgentId,
    summaries: &[AgentSummary],
) -> Vec<AgentId> {
    let mut children: HashMap<AgentId, Vec<&AgentSummary>> = HashMap::new();
    for summary in summaries {
        if let Some(parent_id) = summary.parent_id {
            children.entry(parent_id).or_default().push(summary);
        }
    }
    for values in children.values_mut() {
        values.sort_by_key(|summary| summary.created_at);
    }

    let mut order = Vec::new();
    push_delete_order(root_id, &children, &mut order);
    order
}

fn push_delete_order(
    agent_id: AgentId,
    children: &HashMap<AgentId, Vec<&AgentSummary>>,
    order: &mut Vec<AgentId>,
) {
    if let Some(child_summaries) = children.get(&agent_id) {
        for child in child_summaries {
            push_delete_order(child.id, children, order);
        }
    }
    order.push(agent_id);
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use mai_protocol::{AgentStatus, AgentSummary, TokenUsage, now};
    use uuid::Uuid;

    use super::*;

    fn agent_summary_at(
        agent_id: AgentId,
        parent_id: Option<AgentId>,
        created_at: DateTime<Utc>,
    ) -> AgentSummary {
        AgentSummary {
            id: agent_id,
            parent_id,
            task_id: None,
            project_id: None,
            role: None,
            name: "agent".to_string(),
            status: AgentStatus::Idle,
            container_id: None,
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "mock".to_string(),
            provider_name: "Mock".to_string(),
            model: "mock-model".to_string(),
            reasoning_effort: Some("medium".to_string()),
            created_at,
            updated_at: created_at,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        }
    }

    #[test]
    fn descendant_delete_order_deletes_children_before_parents() {
        let parent = Uuid::new_v4();
        let older_child = Uuid::new_v4();
        let younger_child = Uuid::new_v4();
        let grandchild = Uuid::new_v4();
        let unrelated = Uuid::new_v4();
        let base = now();
        let summaries = vec![
            agent_summary_at(parent, None, base),
            agent_summary_at(
                younger_child,
                Some(parent),
                base + chrono::Duration::seconds(2),
            ),
            agent_summary_at(
                older_child,
                Some(parent),
                base + chrono::Duration::seconds(1),
            ),
            agent_summary_at(
                grandchild,
                Some(older_child),
                base + chrono::Duration::seconds(3),
            ),
            agent_summary_at(unrelated, None, base + chrono::Duration::seconds(4)),
        ];

        assert_eq!(
            descendant_delete_order_from_summaries(parent, &summaries),
            vec![grandchild, older_child, younger_child, parent]
        );
    }
}
