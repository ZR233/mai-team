use std::future::Future;

use mai_protocol::AgentId;

use super::{AgentPurgeOps, purge_agent_tree};
use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CanonicalAgentClose {
    Closed,
    Missing,
}

/// 统一 agent 删除时 canonical runtime 与产品资源之间的生命周期边界。
///
/// framework 中存在 agent 时，由 framework lifecycle adapter 关闭产品资源；若 canonical
/// 状态缺失，实现必须允许本层回退关闭容器和 workspace。无论走哪条路径，只有资源关闭成功后
/// 才能清除持久化产品记录，从而让失败保留为可重试状态。
pub(crate) trait AgentDeleteOps: AgentPurgeOps {
    fn close_canonical_agent(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<CanonicalAgentClose>> + Send;

    fn close_product_agent_resources(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<()>> + Send;

    fn cleanup_agent_workspace(&self, agent_id: AgentId)
    -> impl Future<Output = Result<()>> + Send;
}

pub(crate) async fn delete_agent(ops: &impl AgentDeleteOps, agent_id: AgentId) -> Result<()> {
    match ops.close_canonical_agent(agent_id).await? {
        CanonicalAgentClose::Closed => purge_agent_tree(ops, agent_id).await,
        CanonicalAgentClose::Missing => rollback_unregistered_agent(ops, agent_id).await,
    }
}

pub(crate) async fn rollback_unregistered_agent(
    ops: &impl AgentDeleteOps,
    agent_id: AgentId,
) -> Result<()> {
    ops.close_product_agent_resources(agent_id).await?;
    ops.cleanup_agent_workspace(agent_id).await?;
    purge_agent_tree(ops, agent_id).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{DateTime, Utc};
    use mai_protocol::{AgentState, AgentSummary, TokenUsage};
    use pretty_assertions::assert_eq;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use super::*;

    struct FakeDeleteOps {
        summary: AgentSummary,
        canonical_close: CanonicalAgentClose,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl AgentPurgeOps for FakeDeleteOps {
        async fn agent_summaries(&self) -> Vec<AgentSummary> {
            vec![self.summary.clone()]
        }

        async fn delete_agent_from_store(&self, _agent_id: AgentId) -> Result<()> {
            self.calls.lock().await.push("store");
            Ok(())
        }

        async fn delete_agent_artifacts(&self, _agent_id: AgentId) -> Result<()> {
            self.calls.lock().await.push("artifacts");
            Ok(())
        }

        async fn remove_agent_from_memory(&self, _agent_id: AgentId) {
            self.calls.lock().await.push("memory");
        }

        async fn publish_agent_deleted(&self, _agent_id: AgentId) {
            self.calls.lock().await.push("event");
        }
    }

    impl AgentDeleteOps for FakeDeleteOps {
        async fn close_canonical_agent(&self, _agent_id: AgentId) -> Result<CanonicalAgentClose> {
            self.calls.lock().await.push("canonical");
            Ok(self.canonical_close)
        }

        async fn close_product_agent_resources(&self, _agent_id: AgentId) -> Result<()> {
            self.calls.lock().await.push("product");
            Ok(())
        }

        async fn cleanup_agent_workspace(&self, _agent_id: AgentId) -> Result<()> {
            self.calls.lock().await.push("workspace");
            Ok(())
        }
    }

    #[tokio::test]
    async fn missing_canonical_agent_falls_back_to_full_product_cleanup() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let ops = FakeDeleteOps {
            summary: summary(Uuid::new_v4(), Utc::now()),
            canonical_close: CanonicalAgentClose::Missing,
            calls: Arc::clone(&calls),
        };

        delete_agent(&ops, ops.summary.id)
            .await
            .expect("delete orphan product agent");

        assert_eq!(
            vec![
                "canonical",
                "product",
                "workspace",
                "artifacts",
                "store",
                "memory",
                "event",
            ],
            *calls.lock().await
        );
    }

    #[tokio::test]
    async fn canonical_close_owns_resource_cleanup_before_product_purge() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let ops = FakeDeleteOps {
            summary: summary(Uuid::new_v4(), Utc::now()),
            canonical_close: CanonicalAgentClose::Closed,
            calls: Arc::clone(&calls),
        };

        delete_agent(&ops, ops.summary.id)
            .await
            .expect("delete canonical agent");

        assert_eq!(
            vec!["canonical", "artifacts", "store", "memory", "event"],
            *calls.lock().await
        );
    }

    fn summary(id: AgentId, created_at: DateTime<Utc>) -> AgentSummary {
        AgentSummary {
            id,
            parent_id: None,
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
