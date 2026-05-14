use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use mai_docker::{ContainerCreateOptions, ContainerHandle, DockerClient, project_workspace_volume};
use mai_mcp::McpAgentManager;
use mai_protocol::{AgentId, ProjectId};
use mai_protocol::{McpServerConfig, McpServerScope, McpServerTransport};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::projects::service;
use crate::state::{AgentRecord, RuntimeState};
use crate::turn::tools::ToolExecution;
use crate::{Result, RuntimeError};

pub(crate) const PROJECT_WORKSPACE_PATH: &str = "/workspace/repo";
pub(crate) const PROJECT_GITHUB_MCP_SERVER: &str = "github";
pub(crate) const PROJECT_GIT_MCP_SERVER: &str = "git";

pub(crate) fn project_mcp_configs(token: &str) -> BTreeMap<String, McpServerConfig> {
    let mut configs = BTreeMap::new();
    configs.insert(
        PROJECT_GITHUB_MCP_SERVER.to_string(),
        McpServerConfig {
            scope: McpServerScope::Project,
            transport: McpServerTransport::Stdio,
            command: Some("github-mcp-server".to_string()),
            args: vec!["stdio".to_string()],
            env: BTreeMap::from([
                (
                    "GITHUB_PERSONAL_ACCESS_TOKEN".to_string(),
                    token.to_string(),
                ),
                (
                    "GITHUB_TOOLSETS".to_string(),
                    "context,repos,issues,pull_requests".to_string(),
                ),
            ]),
            enabled: true,
            startup_timeout_secs: Some(20),
            ..McpServerConfig::default()
        },
    );
    configs.insert(
        PROJECT_GIT_MCP_SERVER.to_string(),
        McpServerConfig {
            scope: McpServerScope::Project,
            transport: McpServerTransport::Stdio,
            command: Some("uvx".to_string()),
            args: vec![
                "mcp-server-git".to_string(),
                "--repository".to_string(),
                PROJECT_WORKSPACE_PATH.to_string(),
            ],
            env: BTreeMap::new(),
            enabled: true,
            startup_timeout_secs: Some(20),
            ..McpServerConfig::default()
        },
    );
    configs
}

pub(crate) async fn ensure_sidecar(
    state: &RuntimeState,
    docker: &DockerClient,
    sidecar_image: &str,
    project_id: ProjectId,
) -> Result<ContainerHandle> {
    let project = service::project(state, project_id).await?;
    if let Some(container) = project.sidecar.read().await.clone() {
        return Ok(container);
    }

    let mut sidecar_guard = project.sidecar.write().await;
    if let Some(container) = sidecar_guard.clone() {
        return Ok(container);
    }

    let workspace_volume = project_workspace_volume(&project_id.to_string());
    let container = docker
        .ensure_project_sidecar_container(
            &project_id.to_string(),
            None,
            sidecar_image,
            &workspace_volume,
            &ContainerCreateOptions::default(),
        )
        .await?;
    *sidecar_guard = Some(container.clone());
    Ok(container)
}

pub(crate) async fn shutdown_manager(state: &RuntimeState, project_id: ProjectId) {
    if let Some(manager) = state.project_mcp_managers.write().await.remove(&project_id) {
        manager.shutdown().await;
    }
}

pub(crate) async fn delete_sidecar(
    state: &RuntimeState,
    docker: &DockerClient,
    project_id: ProjectId,
) -> Result<Vec<String>> {
    let project = match service::project(state, project_id).await {
        Ok(project) => project,
        Err(RuntimeError::ProjectNotFound(_)) => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let preferred_container_id = project
        .sidecar
        .write()
        .await
        .take()
        .map(|container| container.id);
    let deleted = docker
        .delete_project_sidecar_containers(
            &project_id.to_string(),
            preferred_container_id.as_deref(),
        )
        .await?;
    if !deleted.is_empty() {
        tracing::info!(
            project_id = %project_id,
            count = deleted.len(),
            "removed project sidecar containers"
        );
    }
    Ok(deleted)
}

pub(crate) async fn cached_manager(
    state: &RuntimeState,
    project_id: ProjectId,
) -> Option<Arc<McpAgentManager>> {
    state
        .project_mcp_managers
        .read()
        .await
        .get(&project_id)
        .cloned()
}

pub(crate) async fn ensure_manager(
    state: &RuntimeState,
    docker: &DockerClient,
    sidecar_image: &str,
    project_id: ProjectId,
    token: Option<&str>,
    cancellation_token: &CancellationToken,
) -> Result<Option<Arc<McpAgentManager>>> {
    if cancellation_token.is_cancelled() {
        return Err(RuntimeError::TurnCancelled);
    }
    if let Some(manager) = cached_manager(state, project_id).await {
        return Ok(Some(manager));
    }

    let Some(token) = token else {
        return Ok(None);
    };
    let sidecar = ensure_sidecar(state, docker, sidecar_image, project_id).await?;
    let configs = project_mcp_configs(token);
    let manager = McpAgentManager::start(docker.clone(), sidecar.id, configs).await;
    if cancellation_token.is_cancelled() {
        manager.shutdown().await;
        return Err(RuntimeError::TurnCancelled);
    }
    let manager = Arc::new(manager);
    let mut managers = state.project_mcp_managers.write().await;
    if let Some(existing) = managers.get(&project_id).cloned() {
        manager.shutdown().await;
        return Ok(Some(existing));
    }
    managers.insert(project_id, Arc::clone(&manager));
    Ok(Some(manager))
}

/// Provides the project-scoped MCP manager and git token needed to execute a
/// project MCP model tool without exposing the full runtime.
pub(crate) trait ProjectMcpToolOps: Send + Sync {
    fn project_mcp_manager_for_agent(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> impl Future<Output = Result<Option<Arc<McpAgentManager>>>> + Send;

    fn project_git_token_for_agent(
        &self,
        agent: &AgentRecord,
    ) -> impl Future<Output = Result<Option<String>>> + Send;
}

pub(crate) async fn execute_project_mcp_tool(
    ops: &impl ProjectMcpToolOps,
    agent: &AgentRecord,
    model_name: &str,
    arguments: Value,
    cancellation_token: CancellationToken,
) -> Result<ToolExecution> {
    let agent_id = agent.summary.read().await.id;
    let Some(manager) = ops
        .project_mcp_manager_for_agent(agent, agent_id, &cancellation_token)
        .await?
    else {
        return Err(RuntimeError::InvalidInput(
            "project MCP manager is not available".to_string(),
        ));
    };
    let token = ops
        .project_git_token_for_agent(agent)
        .await?
        .unwrap_or_default();
    let summary = agent.summary.read().await.clone();
    let arguments = super::review::project_review_mcp_arguments_with_model_footer(
        model_name,
        arguments,
        summary.role.as_ref(),
        &summary.model,
    );
    let output = tokio::select! {
        output = manager.call_model_tool(model_name, arguments) => output,
        _ = cancellation_token.cancelled() => {
            return Err(RuntimeError::TurnCancelled);
        }
    };
    let output = output.map_err(|err| match err {
        mai_mcp::McpError::ToolNotFound(_) => RuntimeError::InvalidInput(format!(
            "project MCP tool `{model_name}` was not discovered"
        )),
        other => RuntimeError::InvalidInput(redact_secret(&other.to_string(), &token)),
    })?;
    Ok(ToolExecution::new(
        true,
        redact_secret(&output.to_string(), &token),
        false,
    ))
}

fn redact_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        return value.to_string();
    }
    value.replace(secret, "<redacted>")
}
