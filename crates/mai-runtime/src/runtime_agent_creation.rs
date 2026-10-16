use std::sync::{Arc, Weak};

use mai_protocol::{AgentId, AgentSummary};

use crate::{AgentRuntime, Result, RuntimeError, agents};

#[derive(Debug, Clone, Copy)]
enum AgentCreationRollbackScope {
    ProductResources,
    CanonicalRuntime,
}

/// 创建中的 agent 资源所有者。
///
/// 正常错误路径必须显式等待 `rollback`，确保异步资源释放已经完成；若创建 future 被取消，
/// `Drop` 会在当前 Tokio runtime 中补交同一项幂等回滚。产品资源成功交给 framework 或调用方后，
/// 必须调用 `commit` 解除回滚责任。
struct AgentCreationLease {
    runtime: Weak<AgentRuntime>,
    agent_id: AgentId,
    rollback_scope: AgentCreationRollbackScope,
    armed: bool,
}

impl AgentCreationLease {
    fn new(runtime: &Arc<AgentRuntime>, agent_id: AgentId) -> Self {
        Self {
            runtime: Arc::downgrade(runtime),
            agent_id,
            rollback_scope: AgentCreationRollbackScope::ProductResources,
            armed: true,
        }
    }

    fn include_canonical_runtime(&mut self) {
        self.rollback_scope = AgentCreationRollbackScope::CanonicalRuntime;
    }

    fn commit(mut self) {
        self.armed = false;
    }

    async fn rollback(mut self) -> Result<()> {
        let runtime = self.runtime.upgrade().ok_or_else(|| {
            RuntimeError::InvalidInput("agent creation owner lost its runtime".to_string())
        })?;
        let result = rollback_agent_creation(runtime, self.agent_id, self.rollback_scope).await;
        if result.is_ok() {
            self.armed = false;
        }
        result
    }
}

impl Drop for AgentCreationLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(runtime) = self.runtime.upgrade() else {
            tracing::warn!(
                agent_id = %self.agent_id,
                "agent creation rollback was abandoned after runtime shutdown"
            );
            return;
        };
        let agent_id = self.agent_id;
        let rollback_scope = self.rollback_scope;
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                agent_id = %agent_id,
                "agent creation rollback was abandoned without a Tokio runtime"
            );
            return;
        };
        handle.spawn(async move {
            if let Err(error) = rollback_agent_creation(runtime, agent_id, rollback_scope).await {
                tracing::warn!(
                    agent_id = %agent_id,
                    "dropped agent creation owner failed to roll back resources: {error}"
                );
            }
        });
    }
}

/// 已完成产品资源准备、尚未移交生命周期责任的 agent。
pub(crate) struct PreparedAgentResource {
    summary: AgentSummary,
    lease: AgentCreationLease,
}

impl PreparedAgentResource {
    pub(crate) fn new(runtime: &Arc<AgentRuntime>, summary: AgentSummary) -> Self {
        Self {
            lease: AgentCreationLease::new(runtime, summary.id),
            summary,
        }
    }

    pub(crate) fn id(&self) -> AgentId {
        self.summary.id
    }

    /// 该 Agent 的父身份快照；根 Agent 为 `None`。
    ///
    /// 注册提交前用它做非阻塞的父退役检查：删除流程会先把整棵子树的节点打上退休标记，
    /// 因此这里只需要读一个内存标记，不需要（也不能）去取父节点的 lifecycle 锁。
    pub(crate) fn parent_id(&self) -> Option<AgentId> {
        self.summary.parent_id
    }

    pub(crate) fn include_canonical_runtime(&mut self) {
        self.lease.include_canonical_runtime();
    }

    pub(crate) fn replace_summary(&mut self, summary: AgentSummary) {
        debug_assert_eq!(self.summary.id, summary.id);
        self.summary = summary;
    }

    pub(crate) fn commit(self) -> AgentSummary {
        self.lease.commit();
        self.summary
    }

    pub(crate) async fn rollback(self) -> Result<()> {
        self.lease.rollback().await
    }
}

async fn rollback_agent_creation(
    runtime: Arc<AgentRuntime>,
    agent_id: AgentId,
    scope: AgentCreationRollbackScope,
) -> Result<()> {
    let result = match scope {
        AgentCreationRollbackScope::ProductResources => {
            agents::rollback_unregistered_agent(runtime.as_ref(), agent_id).await
        }
        AgentCreationRollbackScope::CanonicalRuntime => {
            agents::delete_agent(runtime.as_ref(), agent_id).await
        }
    };
    match result {
        Ok(()) | Err(RuntimeError::AgentNotFound(_)) => Ok(()),
        Err(error) => Err(error),
    }
}
