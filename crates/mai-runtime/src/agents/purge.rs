use std::collections::HashMap;
use std::future::Future;

use mai_protocol::{AgentId, AgentSummary};

use crate::{Result, RuntimeError};

/// 在 PL 完成 agent 树关闭后，清除 mai 产品记录、索引和删除事件。
///
/// 实现必须先持久化删除，再移除内存记录；重复删除持久化记录应保持幂等。
/// 本端口不参与 turn、session 或 lifecycle 状态迁移，这些状态仍由 PL 独占。
pub(crate) trait AgentPurgeOps: Send + Sync {
    fn agent_summaries(&self) -> impl Future<Output = Vec<AgentSummary>> + Send;

    fn delete_agent_from_store(&self, agent_id: AgentId)
    -> impl Future<Output = Result<()>> + Send;

    fn remove_agent_from_memory(&self, agent_id: AgentId) -> impl Future<Output = ()> + Send;

    fn publish_agent_deleted(&self, agent_id: AgentId) -> impl Future<Output = ()> + Send;
}

pub(crate) async fn purge_agent_tree(ops: &impl AgentPurgeOps, root_id: AgentId) -> Result<()> {
    let summaries = ops.agent_summaries().await;
    let targets = descendant_delete_order(root_id, &summaries)?;
    for agent_id in targets {
        ops.delete_agent_from_store(agent_id).await?;
        ops.remove_agent_from_memory(agent_id).await;
        ops.publish_agent_deleted(agent_id).await;
    }
    Ok(())
}

fn descendant_delete_order(root_id: AgentId, summaries: &[AgentSummary]) -> Result<Vec<AgentId>> {
    if !summaries.iter().any(|summary| summary.id == root_id) {
        return Err(RuntimeError::AgentNotFound(root_id));
    }

    let mut children: HashMap<AgentId, Vec<&AgentSummary>> = HashMap::new();
    for summary in summaries {
        if let Some(parent_id) = summary.parent_id {
            children.entry(parent_id).or_default().push(summary);
        }
    }
    for children in children.values_mut() {
        children.sort_by_key(|summary| summary.created_at);
    }

    let mut order = Vec::new();
    push_delete_order(root_id, &children, &mut order);
    Ok(order)
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
    use mai_protocol::{AgentState, TokenUsage};
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn descendant_delete_order_is_child_first_and_stable() {
        let root = Uuid::new_v4();
        let older_child = Uuid::new_v4();
        let younger_child = Uuid::new_v4();
        let grandchild = Uuid::new_v4();
        let unrelated = Uuid::new_v4();
        let base = Utc::now();
        let summaries = vec![
            summary(root, None, base),
            summary(
                younger_child,
                Some(root),
                base + chrono::Duration::seconds(2),
            ),
            summary(older_child, Some(root), base + chrono::Duration::seconds(1)),
            summary(
                grandchild,
                Some(older_child),
                base + chrono::Duration::seconds(3),
            ),
            summary(unrelated, None, base + chrono::Duration::seconds(4)),
        ];

        assert_eq!(
            descendant_delete_order(root, &summaries).expect("delete order"),
            vec![grandchild, older_child, younger_child, root]
        );
    }

    fn summary(id: AgentId, parent_id: Option<AgentId>, created_at: DateTime<Utc>) -> AgentSummary {
        AgentSummary {
            id,
            parent_id,
            task_id: None,
            project_id: None,
            role: None,
            name: "agent".to_string(),
            state: AgentState::default(),
            container_id: None,
            docker_image: "unused".to_string(),
            provider_id: "mock".to_string(),
            provider_name: "Mock".to_string(),
            model: "mock-model".to_string(),
            reasoning_effort: None,
            created_at,
            updated_at: created_at,
            token_usage: TokenUsage::default(),
        }
    }
}
