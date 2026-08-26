use std::future::Future;
use std::sync::Arc;

use crate::mcp::ContainerMcpRuntime;
use mai_docker::ContainerHandle;
use mai_protocol::{AgentId, AgentResourceState, McpStartupStatus, now};

use crate::mcp::ContainerMcpSettings;
use crate::projects::review::context::ProjectRepositoryView;
use crate::projects::workspace::{ProjectRepositoryReviewTarget, ProjectRepositoryRevision};
use crate::state::AgentRecord;
use crate::{Result, RuntimeError};

#[derive(Debug, Clone)]
pub(crate) enum ContainerSource {
    FreshImage,
    ProjectReviewWorkspace {
        target: ProjectRepositoryReviewTarget,
        revision: ProjectRepositoryRevision,
        repository_view: ProjectRepositoryView,
    },
    ProjectWorkspace {
        workspace_volume: String,
        repo_path: String,
        repository_view: Option<ProjectRepositoryView>,
    },
    CloneFrom {
        parent_container_id: String,
        docker_image: String,
        workspace_volume: Option<String>,
    },
}

pub(crate) struct AgentContainerStartRequest {
    pub(crate) agent_id: AgentId,
    pub(crate) preferred_container_id: Option<String>,
    pub(crate) docker_image: String,
    pub(crate) source: ContainerSource,
}

pub(crate) struct AgentMcpStatusChange {
    pub(crate) agent_id: AgentId,
    pub(crate) server: String,
    pub(crate) status: McpStartupStatus,
    pub(crate) error: Option<String>,
}

pub(crate) struct AgentContainerStatusChange {
    pub(crate) state: AgentResourceState,
    pub(crate) error: Option<String>,
}

/// Provides Docker, MCP, persistence, and event side effects required while
/// ensuring an agent container exists.
pub(crate) trait AgentContainerOps: Send + Sync {
    fn start_agent_container(
        &self,
        request: AgentContainerStartRequest,
    ) -> impl Future<Output = Result<ContainerHandle>> + Send;

    fn remove_agent_container(
        &self,
        agent_id: AgentId,
        container_id: String,
    ) -> impl Future<Output = ()> + Send;

    fn agent_mcp_runtime_config(
        &self,
        agent: &AgentRecord,
    ) -> impl Future<Output = Result<ContainerMcpSettings>> + Send;

    fn start_agent_mcp_runtime(
        &self,
        agent_id: AgentId,
        container_id: String,
        config: ContainerMcpSettings,
    ) -> impl Future<Output = Result<ContainerMcpRuntime>> + Send;

    fn set_agent_resource_state(
        &self,
        agent: Arc<AgentRecord>,
        change: AgentContainerStatusChange,
    ) -> impl Future<Output = Result<()>> + Send;

    fn persist_agent(&self, agent: Arc<AgentRecord>) -> impl Future<Output = Result<()>> + Send;

    fn publish_mcp_status(&self, change: AgentMcpStatusChange) -> impl Future<Output = ()> + Send;
}

pub(crate) async fn ensure_agent_container_with_source(
    ops: &impl AgentContainerOps,
    agent: &Arc<AgentRecord>,
    container_source: &ContainerSource,
) -> Result<String> {
    if let Some(container_id) = agent
        .container
        .read()
        .await
        .as_ref()
        .map(|container| container.id.clone())
    {
        return Ok(container_id);
    }

    let (agent_id, preferred_container_id, docker_image) = {
        let summary = agent.summary.read().await;
        (
            summary.id,
            summary.container_id.clone(),
            summary.docker_image.clone(),
        )
    };
    let mut container_guard = agent.container.write().await;
    if let Some(container_id) = container_guard
        .as_ref()
        .map(|container| container.id.clone())
    {
        return Ok(container_id);
    }

    // 容器缺失即代表 transport identity 已变化；必须先关闭旧 MCP handle，
    // 避免旧 stdio 进程或 HTTP session 在新容器 generation 建立后继续存活。
    if let Some(previous) = agent.mcp.write().await.take() {
        previous.shutdown().await;
    }

    set_resource_state(ops, agent, AgentResourceState::Provisioning, None).await?;
    let container = match ops
        .start_agent_container(AgentContainerStartRequest {
            agent_id,
            preferred_container_id,
            docker_image,
            source: container_source.clone(),
        })
        .await
    {
        Ok(container) => container,
        Err(err) => {
            let message = err.to_string();
            drop(container_guard);
            if let Err(store_err) =
                set_resource_state(ops, agent, AgentResourceState::Failed, Some(message)).await
            {
                tracing::warn!("failed to persist container startup failure: {store_err}");
            }
            return Err(err);
        }
    };

    let container_id = container.id.clone();
    let setup: Result<ContainerMcpRuntime> = async {
        {
            let mut summary = agent.summary.write().await;
            summary.container_id = Some(container_id.clone());
            summary.updated_at = now();
        }
        ops.persist_agent(Arc::clone(agent)).await?;
        *container_guard = Some(container.clone());
        drop(container_guard);

        let mcp_config = ops.agent_mcp_runtime_config(agent).await?;
        for server in mcp_config
            .user_servers
            .iter()
            .filter_map(|(server, config)| config.enabled.then_some(server))
        {
            ops.publish_mcp_status(AgentMcpStatusChange {
                agent_id,
                server: server.clone(),
                status: McpStartupStatus::Starting,
                error: None,
            })
            .await;
        }
        let mcp = ops
            .start_agent_mcp_runtime(agent_id, container.id, mcp_config)
            .await?;
        for status in mcp.statuses().await {
            ops.publish_mcp_status(AgentMcpStatusChange {
                agent_id,
                server: status.server,
                status: status.status,
                error: status.error,
            })
            .await;
        }
        let required_failures = mcp.required_failures().await;
        if !required_failures.is_empty() {
            let message = required_failures
                .iter()
                .map(|status| {
                    format!(
                        "{}: {}",
                        status.server,
                        status
                            .error
                            .as_deref()
                            .unwrap_or("required MCP server failed")
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            mcp.shutdown().await;
            return Err(RuntimeError::InvalidInput(format!(
                "required MCP server startup failed: {message}"
            )));
        }
        if let Err(error) = set_resource_state(ops, agent, AgentResourceState::Ready, None).await {
            mcp.shutdown().await;
            return Err(error);
        }
        Ok(mcp)
    }
    .await;

    match setup {
        Ok(mcp) => {
            *agent.mcp.write().await = Some(Arc::new(mcp));
            Ok(container_id)
        }
        Err(error) => {
            let failure = error.to_string();
            *agent.container.write().await = None;
            {
                let mut summary = agent.summary.write().await;
                summary.container_id = None;
            }
            ops.remove_agent_container(agent_id, container_id).await;
            if let Err(persist_error) = set_resource_state(
                ops,
                agent,
                AgentResourceState::Failed,
                Some(failure.clone()),
            )
            .await
            {
                return Err(RuntimeError::InvalidInput(format!(
                    "agent container startup failed: {failure}; failure persistence failed: {persist_error}"
                )));
            }
            Err(error)
        }
    }
}

async fn set_resource_state(
    ops: &impl AgentContainerOps,
    agent: &Arc<AgentRecord>,
    state: AgentResourceState,
    error: Option<String>,
) -> Result<()> {
    ops.set_agent_resource_state(
        Arc::clone(agent),
        AgentContainerStatusChange { state, error },
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use mai_protocol::{AgentResourceSnapshot, TokenUsage};
    use pretty_assertions::assert_eq;
    use tokio::sync::RwLock;
    use uuid::Uuid;

    use super::*;

    #[derive(Clone, Copy)]
    enum FailurePoint {
        Persistence,
        McpConfig,
    }

    struct ContainerFailureOps {
        removed: AtomicUsize,
        failure_point: FailurePoint,
    }

    impl AgentContainerOps for ContainerFailureOps {
        async fn start_agent_container(
            &self,
            request: AgentContainerStartRequest,
        ) -> Result<ContainerHandle> {
            Ok(ContainerHandle {
                id: "new-container".to_string(),
                name: format!("mai-team-{}", request.agent_id),
                image: request.docker_image,
            })
        }

        async fn remove_agent_container(&self, _agent_id: AgentId, _container_id: String) {
            self.removed.fetch_add(1, Ordering::AcqRel);
        }

        async fn agent_mcp_runtime_config(
            &self,
            _agent: &AgentRecord,
        ) -> Result<ContainerMcpSettings> {
            match self.failure_point {
                FailurePoint::Persistence => {
                    Err(RuntimeError::InvalidInput("unreachable".to_string()))
                }
                FailurePoint::McpConfig => {
                    Err(RuntimeError::InvalidInput("MCP config failed".to_string()))
                }
            }
        }

        async fn start_agent_mcp_runtime(
            &self,
            _agent_id: AgentId,
            _container_id: String,
            _config: ContainerMcpSettings,
        ) -> Result<ContainerMcpRuntime> {
            Err(RuntimeError::InvalidInput("unreachable".to_string()))
        }

        async fn set_agent_resource_state(
            &self,
            agent: Arc<AgentRecord>,
            change: AgentContainerStatusChange,
        ) -> Result<()> {
            let mut summary = agent.summary.write().await;
            summary.resource.state = change.state;
            summary.resource.error = change.error;
            Ok(())
        }

        async fn persist_agent(&self, _agent: Arc<AgentRecord>) -> Result<()> {
            match self.failure_point {
                FailurePoint::Persistence => {
                    Err(RuntimeError::InvalidInput("persist failed".to_string()))
                }
                FailurePoint::McpConfig => Ok(()),
            }
        }

        async fn publish_mcp_status(&self, _change: AgentMcpStatusChange) {}
    }

    #[tokio::test]
    async fn persistence_failure_after_container_creation_releases_container() {
        let ops = ContainerFailureOps {
            removed: AtomicUsize::new(0),
            failure_point: FailurePoint::Persistence,
        };
        let agent = Arc::new(agent_record());

        ensure_agent_container_with_source(&ops, &agent, &ContainerSource::FreshImage)
            .await
            .expect_err("persist must fail");

        assert_eq!(ops.removed.load(Ordering::Acquire), 1);
        assert!(agent.container.read().await.is_none());
        let summary = agent.summary.read().await.clone();
        assert_eq!(summary.container_id, None);
        assert_eq!(summary.resource.state, AgentResourceState::Failed);
        assert_eq!(
            summary.resource.error,
            Some("invalid input: persist failed".to_string())
        );
    }

    #[tokio::test]
    async fn mcp_setup_failure_releases_persisted_container_generation() {
        let ops = ContainerFailureOps {
            removed: AtomicUsize::new(0),
            failure_point: FailurePoint::McpConfig,
        };
        let agent = Arc::new(agent_record());

        ensure_agent_container_with_source(&ops, &agent, &ContainerSource::FreshImage)
            .await
            .expect_err("MCP config must fail");

        assert_eq!(ops.removed.load(Ordering::Acquire), 1);
        assert!(agent.container.read().await.is_none());
        let summary = agent.summary.read().await.clone();
        assert_eq!(summary.container_id, None);
        assert_eq!(summary.resource.state, AgentResourceState::Failed);
        assert_eq!(
            summary.resource.error,
            Some("invalid input: MCP config failed".to_string())
        );
    }

    fn agent_record() -> AgentRecord {
        let timestamp = now();
        AgentRecord {
            summary: RwLock::new(mai_protocol::AgentSummary {
                id: Uuid::new_v4(),
                parent_id: None,
                task_id: None,
                project_id: None,
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
            }),
            container: RwLock::new(None),
            mcp: RwLock::new(None),
            review_context: RwLock::new(None),
            system_prompt: None,
        }
    }
}
