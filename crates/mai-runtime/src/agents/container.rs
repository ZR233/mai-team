use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use crate::mcp::McpAgentManager;
use mai_docker::ContainerHandle;
use mai_protocol::{AgentId, AgentResourceState, McpServerConfig, McpStartupStatus, now};

use crate::projects::workspace::{ProjectRepositoryReviewTarget, ProjectRepositoryRevision};
use crate::state::AgentRecord;
use crate::{Result, RuntimeError};

#[derive(Debug, Clone)]
pub(crate) enum ContainerSource {
    FreshImage,
    ProjectReviewWorkspace {
        target: ProjectRepositoryReviewTarget,
        revision: ProjectRepositoryRevision,
    },
    ProjectWorkspace {
        workspace_volume: String,
        repo_path: String,
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

    fn agent_mcp_server_configs(
        &self,
    ) -> impl Future<Output = Result<BTreeMap<String, McpServerConfig>>> + Send;

    fn start_agent_mcp_manager(
        &self,
        container_id: String,
        configs: BTreeMap<String, McpServerConfig>,
    ) -> impl Future<Output = McpAgentManager> + Send;

    fn set_agent_resource_state(
        &self,
        agent: Arc<AgentRecord>,
        change: AgentContainerStatusChange,
    ) -> impl Future<Output = Result<()>> + Send;

    fn persist_agent(&self, agent: Arc<AgentRecord>) -> impl Future<Output = Result<()>> + Send;

    fn publish_mcp_status(&self, change: AgentMcpStatusChange) -> impl Future<Output = ()> + Send;
}

pub(crate) async fn ensure_agent_container(
    ops: &impl AgentContainerOps,
    agent: &Arc<AgentRecord>,
) -> Result<String> {
    ensure_agent_container_with_source(ops, agent, &ContainerSource::FreshImage).await
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
    {
        let mut summary = agent.summary.write().await;
        summary.container_id = Some(container_id.clone());
        summary.updated_at = now();
    }
    ops.persist_agent(Arc::clone(agent)).await?;
    *container_guard = Some(container.clone());
    drop(container_guard);

    let mcp_configs = ops.agent_mcp_server_configs().await?;
    for server in mcp_configs
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
    let mcp = ops.start_agent_mcp_manager(container.id, mcp_configs).await;
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
        *agent.container.write().await = None;
        {
            let mut summary = agent.summary.write().await;
            summary.container_id = None;
        }
        ops.remove_agent_container(agent_id, container_id).await;
        set_resource_state(
            ops,
            agent,
            AgentResourceState::Failed,
            Some(message.clone()),
        )
        .await?;
        return Err(RuntimeError::InvalidInput(format!(
            "required MCP server startup failed: {message}"
        )));
    }
    *agent.mcp.write().await = Some(Arc::new(mcp));
    set_resource_state(ops, agent, AgentResourceState::Ready, None).await?;
    Ok(container_id)
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
