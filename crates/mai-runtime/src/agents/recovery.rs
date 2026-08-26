use std::future::Future;
use std::sync::{Arc, Weak};

use mai_protocol::{AgentId, AgentResourceState, AgentSummary};

use super::ContainerSource;
use crate::{Result, RuntimeError};

const CREATED_WORKSPACE_RECOVERY_FAILURE_PREFIX: &str =
    "project agent resource recovery failed (created workspace): ";
const BORROWED_WORKSPACE_RECOVERY_FAILURE_PREFIX: &str =
    "project agent resource recovery failed (borrowed workspace): ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentWorkspaceOwnership {
    Borrowed,
    CreatedHere,
}

/// 启动恢复请求，显式声明 workspace 是否由本次恢复创建。
///
/// `CreatedHere` 的 workspace 会在失败或 future 被取消时由 lease 删除；`Borrowed`
/// 只恢复容器，不得删除调用方已经提交的 workspace。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentResourceRecoveryRequest {
    agent_id: AgentId,
    workspace_ownership: AgentWorkspaceOwnership,
}

impl AgentResourceRecoveryRequest {
    pub(crate) fn created_workspace(agent_id: AgentId) -> Self {
        Self {
            agent_id,
            workspace_ownership: AgentWorkspaceOwnership::CreatedHere,
        }
    }

    pub(crate) fn borrowed_workspace(agent_id: AgentId) -> Self {
        Self {
            agent_id,
            workspace_ownership: AgentWorkspaceOwnership::Borrowed,
        }
    }

    pub(crate) fn agent_id(self) -> AgentId {
        self.agent_id
    }

    fn failure_prefix(self) -> &'static str {
        match self.workspace_ownership {
            AgentWorkspaceOwnership::Borrowed => BORROWED_WORKSPACE_RECOVERY_FAILURE_PREFIX,
            AgentWorkspaceOwnership::CreatedHere => CREATED_WORKSPACE_RECOVERY_FAILURE_PREFIX,
        }
    }
}

/// 已准备、尚未启动容器的 agent workspace。
#[derive(Debug, Clone)]
pub(crate) struct PreparedAgentWorkspace {
    pub(crate) source: ContainerSource,
}

/// 恢复既有 Thread 的派生运行资源所需端口。
///
/// 实现必须让关闭容器和删除 workspace 保持幂等；`prepare_agent_workspace` 可以创建
/// workspace，但不得修改 canonical Thread 或产品 Agent 身份；`mark_recovery_failed`
/// 只能更新派生资源状态，不得清除消息历史。
pub(crate) trait AgentResourceRecoveryOps: Send + Sync + 'static {
    fn close_agent_resources(&self, agent_id: AgentId) -> impl Future<Output = Result<()>> + Send;

    fn prepare_agent_workspace(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<PreparedAgentWorkspace>> + Send;

    fn start_agent_container(
        &self,
        agent_id: AgentId,
        workspace: PreparedAgentWorkspace,
    ) -> impl Future<Output = Result<()>> + Send;

    fn cleanup_agent_workspace(&self, agent_id: AgentId)
    -> impl Future<Output = Result<()>> + Send;

    fn mark_recovery_failed(
        &self,
        agent_id: AgentId,
        error: String,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// 既有 Thread 的派生资源恢复所有者。
///
/// canonical Thread 和产品 Agent 始终是 borrowed，不在本 lease 的释放范围内。显式错误
/// 会等待回滚；future 被取消时，`Drop` 会补交同一项幂等回滚。
struct AgentResourceRecoveryLease<O: AgentResourceRecoveryOps> {
    ops: Weak<O>,
    request: AgentResourceRecoveryRequest,
    workspace_preparation_started: bool,
    failure: String,
    armed: bool,
}

impl<O: AgentResourceRecoveryOps> AgentResourceRecoveryLease<O> {
    fn new(ops: &Arc<O>, request: AgentResourceRecoveryRequest) -> Self {
        Self {
            ops: Arc::downgrade(ops),
            request,
            workspace_preparation_started: false,
            failure: format!("{}recovery owner was dropped", request.failure_prefix()),
            armed: true,
        }
    }

    fn begin_workspace_preparation(&mut self) {
        self.workspace_preparation_started = true;
    }

    fn commit(mut self) {
        self.armed = false;
    }

    async fn rollback(mut self, failure: String) -> Result<()> {
        self.failure = failure;
        let ops = self.ops.upgrade().ok_or_else(|| {
            RuntimeError::InvalidInput("agent resource recovery lost its runtime".to_string())
        })?;
        let result = rollback_agent_resource_recovery(
            ops,
            self.request,
            self.workspace_preparation_started,
            self.failure.clone(),
        )
        .await;
        if result.is_ok() {
            self.armed = false;
        }
        result
    }
}

impl<O: AgentResourceRecoveryOps> Drop for AgentResourceRecoveryLease<O> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(ops) = self.ops.upgrade() else {
            tracing::warn!(
                agent_id = %self.request.agent_id,
                "agent resource recovery rollback was abandoned after runtime shutdown"
            );
            return;
        };
        let request = self.request;
        let workspace_preparation_started = self.workspace_preparation_started;
        let failure = self.failure.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                agent_id = %request.agent_id,
                "agent resource recovery rollback was abandoned without a Tokio runtime"
            );
            return;
        };
        handle.spawn(async move {
            if let Err(error) = rollback_agent_resource_recovery(
                ops,
                request,
                workspace_preparation_started,
                failure,
            )
            .await
            {
                tracing::warn!(
                    agent_id = %request.agent_id,
                    "dropped agent resource recovery owner failed to roll back resources: {error}"
                );
            }
        });
    }
}

pub(crate) async fn recover_agent_resources<O: AgentResourceRecoveryOps>(
    ops: Arc<O>,
    request: AgentResourceRecoveryRequest,
) -> Result<()> {
    let mut lease = AgentResourceRecoveryLease::new(&ops, request);
    let recovery = async {
        ops.close_agent_resources(request.agent_id).await?;
        lease.begin_workspace_preparation();
        let workspace = ops.prepare_agent_workspace(request.agent_id).await?;
        ops.start_agent_container(request.agent_id, workspace).await
    }
    .await;

    match recovery {
        Ok(()) => {
            lease.commit();
            Ok(())
        }
        Err(error) => {
            let failure = format!("{}{error}", request.failure_prefix());
            match lease.rollback(failure).await {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(RuntimeError::InvalidInput(format!(
                    "agent resource recovery failed: {error}; rollback failed: {rollback_error}"
                ))),
            }
        }
    }
}

pub(crate) fn agent_resource_recovery_retry_request(
    agent: &AgentSummary,
) -> Option<AgentResourceRecoveryRequest> {
    if agent.resource.state != AgentResourceState::Failed {
        return None;
    }
    let error = agent.resource.error.as_deref()?.trim();
    let error = error.strip_prefix("invalid input: ").unwrap_or(error);
    if error.starts_with(CREATED_WORKSPACE_RECOVERY_FAILURE_PREFIX) {
        Some(AgentResourceRecoveryRequest::created_workspace(agent.id))
    } else if error.starts_with(BORROWED_WORKSPACE_RECOVERY_FAILURE_PREFIX) {
        Some(AgentResourceRecoveryRequest::borrowed_workspace(agent.id))
    } else {
        None
    }
}

async fn rollback_agent_resource_recovery<O: AgentResourceRecoveryOps>(
    ops: Arc<O>,
    request: AgentResourceRecoveryRequest,
    workspace_preparation_started: bool,
    failure: String,
) -> Result<()> {
    let mut rollback_errors = Vec::new();
    if let Err(error) = ops.close_agent_resources(request.agent_id).await {
        rollback_errors.push(format!("container cleanup: {error}"));
    }
    if workspace_preparation_started
        && request.workspace_ownership == AgentWorkspaceOwnership::CreatedHere
        && let Err(error) = ops.cleanup_agent_workspace(request.agent_id).await
    {
        rollback_errors.push(format!("workspace cleanup: {error}"));
    }
    let persisted_failure = if rollback_errors.is_empty() {
        failure
    } else {
        format!("{failure}; rollback failed: {}", rollback_errors.join("; "))
    };
    if let Err(error) = ops
        .mark_recovery_failed(request.agent_id, persisted_failure)
        .await
    {
        rollback_errors.push(format!("failure persistence: {error}"));
    }
    if rollback_errors.is_empty() {
        Ok(())
    } else {
        Err(RuntimeError::InvalidInput(rollback_errors.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use mai_protocol::{AgentResourceSnapshot, TokenUsage, now};
    use pretty_assertions::assert_eq;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use super::*;

    #[derive(Default)]
    struct FakeRecoveryOps {
        calls: Mutex<Vec<&'static str>>,
        fail_start: AtomicBool,
    }

    impl AgentResourceRecoveryOps for FakeRecoveryOps {
        async fn close_agent_resources(&self, _agent_id: AgentId) -> Result<()> {
            self.calls.lock().await.push("close");
            Ok(())
        }

        async fn prepare_agent_workspace(
            &self,
            _agent_id: AgentId,
        ) -> Result<PreparedAgentWorkspace> {
            self.calls.lock().await.push("prepare");
            Ok(PreparedAgentWorkspace {
                source: ContainerSource::FreshImage,
            })
        }

        async fn start_agent_container(
            &self,
            _agent_id: AgentId,
            _workspace: PreparedAgentWorkspace,
        ) -> Result<()> {
            self.calls.lock().await.push("start");
            if self.fail_start.load(Ordering::Acquire) {
                return Err(RuntimeError::InvalidInput("start failed".to_string()));
            }
            Ok(())
        }

        async fn cleanup_agent_workspace(&self, _agent_id: AgentId) -> Result<()> {
            self.calls.lock().await.push("workspace");
            Ok(())
        }

        async fn mark_recovery_failed(&self, _agent_id: AgentId, _error: String) -> Result<()> {
            self.calls.lock().await.push("failed");
            Ok(())
        }
    }

    #[tokio::test]
    async fn failed_recovery_releases_only_new_derived_resources() {
        let ops = Arc::new(FakeRecoveryOps::default());
        ops.fail_start.store(true, Ordering::Release);

        recover_agent_resources(
            Arc::clone(&ops),
            AgentResourceRecoveryRequest::created_workspace(Uuid::new_v4()),
        )
        .await
        .expect_err("recovery must fail");

        assert_eq!(
            *ops.calls.lock().await,
            vec!["close", "prepare", "start", "close", "workspace", "failed"]
        );
    }

    #[tokio::test]
    async fn successful_recovery_commits_without_cleanup() {
        let ops = Arc::new(FakeRecoveryOps::default());

        recover_agent_resources(
            Arc::clone(&ops),
            AgentResourceRecoveryRequest::created_workspace(Uuid::new_v4()),
        )
        .await
        .expect("recovery");

        assert_eq!(*ops.calls.lock().await, vec!["close", "prepare", "start"]);
    }

    #[tokio::test]
    async fn borrowed_workspace_survives_recovery_rollback() {
        let ops = Arc::new(FakeRecoveryOps::default());
        ops.fail_start.store(true, Ordering::Release);

        recover_agent_resources(
            Arc::clone(&ops),
            AgentResourceRecoveryRequest::borrowed_workspace(Uuid::new_v4()),
        )
        .await
        .expect_err("recovery must fail");

        assert_eq!(
            *ops.calls.lock().await,
            vec!["close", "prepare", "start", "close", "failed"]
        );
    }

    #[tokio::test]
    async fn dropped_recovery_owner_submits_idempotent_cleanup() {
        let ops = Arc::new(FakeRecoveryOps::default());
        let mut lease = AgentResourceRecoveryLease::new(
            &ops,
            AgentResourceRecoveryRequest::created_workspace(Uuid::new_v4()),
        );
        lease.begin_workspace_preparation();
        drop(lease);

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if ops.calls.lock().await.len() == 3 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drop cleanup");

        assert_eq!(
            *ops.calls.lock().await,
            vec!["close", "workspace", "failed"]
        );
    }

    #[test]
    fn retry_request_preserves_workspace_ownership() {
        let agent_id = Uuid::new_v4();
        let mut summary = agent_summary(agent_id);
        summary.resource.state = AgentResourceState::Failed;
        summary.resource.error = Some(format!(
            "invalid input: {CREATED_WORKSPACE_RECOVERY_FAILURE_PREFIX}clone failed"
        ));

        assert_eq!(
            agent_resource_recovery_retry_request(&summary),
            Some(AgentResourceRecoveryRequest::created_workspace(agent_id))
        );

        summary.resource.error = Some(format!(
            "{BORROWED_WORKSPACE_RECOVERY_FAILURE_PREFIX}container failed"
        ));
        assert_eq!(
            agent_resource_recovery_retry_request(&summary),
            Some(AgentResourceRecoveryRequest::borrowed_workspace(agent_id))
        );

        summary.resource.state = AgentResourceState::Ready;
        assert_eq!(agent_resource_recovery_retry_request(&summary), None);
    }

    fn agent_summary(agent_id: AgentId) -> AgentSummary {
        let timestamp = now();
        AgentSummary {
            id: agent_id,
            parent_id: None,
            task_id: None,
            project_id: Some(Uuid::new_v4()),
            role: None,
            name: "agent".to_string(),
            resource: AgentResourceSnapshot::default(),
            runtime: None,
            container_id: None,
            docker_image: "image".to_string(),
            provider_id: "provider".to_string(),
            provider_name: "Provider".to_string(),
            model: "model".to_string(),
            reasoning_effort: None,
            created_at: timestamp,
            updated_at: timestamp,
            token_usage: TokenUsage::default(),
        }
    }
}
