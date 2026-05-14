use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use mai_docker::ContainerHandle;
use mai_mcp::McpAgentManager;
use mai_protocol::{AgentId, AgentStatus, McpServerConfig, McpStartupStatus, TurnId, now};
use tokio_util::sync::CancellationToken;

use crate::state::{AgentRecord, TurnGuard};
use crate::{Result, RuntimeError};

#[derive(Debug, Clone)]
pub(crate) enum ContainerSource {
    FreshImage,
    ProjectClone {
        clone_path: String,
    },
    CloneFrom {
        parent_container_id: String,
        docker_image: String,
        workspace_volume: Option<String>,
        repo_mount: Option<String>,
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
    pub(crate) status: AgentStatus,
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

    fn set_agent_status(
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
    ready_status: AgentStatus,
) -> Result<String> {
    ensure_agent_container_with_source(ops, agent, ready_status, &ContainerSource::FreshImage, None)
        .await
}

pub(crate) async fn ensure_agent_container_for_turn(
    ops: &impl AgentContainerOps,
    agent: &Arc<AgentRecord>,
    ready_status: AgentStatus,
    turn_id: TurnId,
    cancellation_token: &CancellationToken,
) -> Result<String> {
    if cancellation_token.is_cancelled() {
        return Err(RuntimeError::TurnCancelled);
    }
    let turn_guard =
        (agent.summary.read().await.current_turn == Some(turn_id)).then(|| TurnGuard {
            turn_id,
            cancellation_token: cancellation_token.clone(),
        });
    let container_id = ensure_agent_container_with_source(
        ops,
        agent,
        ready_status.clone(),
        &ContainerSource::FreshImage,
        turn_guard,
    )
    .await?;
    let current_turn = agent.summary.read().await.current_turn;
    if cancellation_token.is_cancelled() || current_turn.is_some_and(|current| current != turn_id) {
        if let Some(manager) = agent.mcp.write().await.take() {
            manager.shutdown().await;
        }
        return Err(RuntimeError::TurnCancelled);
    }
    let needs_status_restore = agent.summary.read().await.status != ready_status;
    if needs_status_restore {
        set_status(ops, agent, ready_status, None).await?;
    }
    Ok(container_id)
}

pub(crate) async fn ensure_agent_container_with_source(
    ops: &impl AgentContainerOps,
    agent: &Arc<AgentRecord>,
    ready_status: AgentStatus,
    container_source: &ContainerSource,
    turn_guard: Option<TurnGuard>,
) -> Result<String> {
    if let Some(guard) = &turn_guard {
        ensure_turn_current(agent, guard).await?;
    }
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

    set_status(ops, agent, AgentStatus::StartingContainer, None).await?;
    if let Some(guard) = &turn_guard {
        ensure_turn_current(agent, guard).await?;
    }
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
            if let Err(store_err) = set_status(ops, agent, AgentStatus::Failed, Some(message)).await
            {
                tracing::warn!("failed to persist container startup failure: {store_err}");
            }
            return Err(err);
        }
    };

    let container_id = container.id.clone();
    if let Some(guard) = &turn_guard
        && let Err(err) = ensure_turn_current(agent, guard).await
    {
        drop(container_guard);
        ops.remove_agent_container(agent_id, container_id).await;
        return Err(err);
    }
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
    if let Some(guard) = &turn_guard
        && let Err(err) = ensure_turn_current(agent, guard).await
    {
        mcp.shutdown().await;
        *agent.container.write().await = None;
        {
            let mut summary = agent.summary.write().await;
            summary.container_id = None;
        }
        ops.remove_agent_container(agent_id, container_id).await;
        return Err(err);
    }
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
        set_status(ops, agent, AgentStatus::Failed, Some(message.clone())).await?;
        return Err(RuntimeError::InvalidInput(format!(
            "required MCP server startup failed: {message}"
        )));
    }
    if let Some(guard) = &turn_guard {
        ensure_turn_current(agent, guard).await?;
    }
    *agent.mcp.write().await = Some(Arc::new(mcp));
    set_status(ops, agent, ready_status, None).await?;
    Ok(container_id)
}

async fn set_status(
    ops: &impl AgentContainerOps,
    agent: &Arc<AgentRecord>,
    status: AgentStatus,
    error: Option<String>,
) -> Result<()> {
    ops.set_agent_status(
        Arc::clone(agent),
        AgentContainerStatusChange { status, error },
    )
    .await
}

async fn ensure_turn_current(agent: &AgentRecord, guard: &TurnGuard) -> Result<()> {
    if guard.cancellation_token.is_cancelled() {
        return Err(RuntimeError::TurnCancelled);
    }
    if agent.summary.read().await.current_turn != Some(guard.turn_id) {
        return Err(RuntimeError::TurnCancelled);
    }
    Ok(())
}
