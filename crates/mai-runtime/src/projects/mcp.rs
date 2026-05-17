use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use mai_docker::{ContainerCreateOptions, ContainerHandle, DockerClient, project_cache_volume};
use mai_mcp::{McpAgentManager, McpTool};
use mai_protocol::McpServerConfig;
use mai_protocol::{AgentId, ProjectId};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::projects::service;
use crate::state::{AgentRecord, RuntimeState};
use crate::turn::tools::ToolExecution;
use crate::{Result, RuntimeError};

pub(crate) const PROJECT_WORKSPACE_PATH: &str = "/workspace/repo";
const PROJECT_MCP_TOKEN_REFRESH_SKEW_SECS: i64 = 120;

#[derive(Debug, Clone)]
pub(crate) struct ProjectMcpCredential {
    pub(crate) token: String,
    pub(crate) expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub(crate) struct ProjectMcpManagerHandle {
    manager: Arc<McpAgentManager>,
    token_expires_at: Option<DateTime<Utc>>,
}

impl ProjectMcpManagerHandle {
    pub(crate) fn with_token_expiry(
        manager: Arc<McpAgentManager>,
        token_expires_at: DateTime<Utc>,
    ) -> Self {
        Self {
            manager,
            token_expires_at: Some(token_expires_at),
        }
    }

    pub(crate) fn without_token_expiry(manager: Arc<McpAgentManager>) -> Self {
        Self {
            manager,
            token_expires_at: None,
        }
    }

    pub(crate) fn manager(&self) -> Arc<McpAgentManager> {
        Arc::clone(&self.manager)
    }

    fn token_expires_soon(&self, now: DateTime<Utc>) -> bool {
        self.token_expires_at.is_some_and(|expires_at| {
            expires_at <= now + TimeDelta::seconds(PROJECT_MCP_TOKEN_REFRESH_SKEW_SECS)
        })
    }
}

pub(crate) fn project_mcp_configs(_token: &str) -> BTreeMap<String, McpServerConfig> {
    BTreeMap::new()
}

pub(crate) fn is_github_mcp_tool(tool: &McpTool) -> bool {
    tool.server == "github" || tool.model_name.starts_with("mcp__github__")
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

    let workspace_volume = project_cache_volume(&project_id.to_string());
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
    if let Some(handle) = state.project_mcp_managers.write().await.remove(&project_id) {
        handle.manager.shutdown().await;
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
    let refresh_needed = {
        let managers = state.project_mcp_managers.read().await;
        match managers.get(&project_id) {
            Some(handle) if !handle.token_expires_soon(Utc::now()) => {
                return Some(handle.manager());
            }
            Some(_) => true,
            None => false,
        }
    };
    if refresh_needed {
        shutdown_manager(state, project_id).await;
    }
    None
}

pub(crate) async fn ensure_manager(
    state: &RuntimeState,
    docker: &DockerClient,
    sidecar_image: &str,
    project_id: ProjectId,
    credential: ProjectMcpCredential,
    cancellation_token: &CancellationToken,
) -> Result<Arc<McpAgentManager>> {
    if cancellation_token.is_cancelled() {
        return Err(RuntimeError::TurnCancelled);
    }
    if let Some(manager) = cached_manager(state, project_id).await {
        return Ok(manager);
    }
    let sidecar = ensure_sidecar(state, docker, sidecar_image, project_id).await?;
    let configs = project_mcp_configs(&credential.token);
    let manager = McpAgentManager::start(docker.clone(), sidecar.id, configs).await;
    if cancellation_token.is_cancelled() {
        manager.shutdown().await;
        return Err(RuntimeError::TurnCancelled);
    }
    let manager = Arc::new(manager);
    let previous = {
        let mut managers = state.project_mcp_managers.write().await;
        match managers.get(&project_id) {
            Some(existing) if !existing.token_expires_soon(Utc::now()) => {
                let existing = existing.manager();
                drop(managers);
                manager.shutdown().await;
                return Ok(existing);
            }
            Some(_) | None => {
                let handle = match credential.expires_at {
                    Some(expires_at) => {
                        ProjectMcpManagerHandle::with_token_expiry(Arc::clone(&manager), expires_at)
                    }
                    None => ProjectMcpManagerHandle::without_token_expiry(Arc::clone(&manager)),
                };
                managers.insert(project_id, handle)
            }
        }
    };
    if let Some(previous) = previous {
        previous.manager.shutdown().await;
    }
    Ok(manager)
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

    fn call_project_mcp_tool(
        &self,
        manager: Arc<McpAgentManager>,
        model_name: String,
        arguments: Value,
    ) -> impl Future<Output = std::result::Result<Value, mai_mcp::McpError>> + Send;
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
    let output = tokio::select! {
        output = ops.call_project_mcp_tool(manager, model_name.to_string(), arguments) => output,
        _ = cancellation_token.cancelled() => {
            return Err(RuntimeError::TurnCancelled);
        }
    };
    match output {
        Ok(output) => Ok(ToolExecution::new(
            true,
            redact_secret(&output.to_string(), &token),
            false,
        )),
        Err(mai_mcp::McpError::ToolNotFound(_)) => Err(RuntimeError::InvalidInput(format!(
            "project MCP tool `{model_name}` was not discovered"
        ))),
        Err(err) => Err(RuntimeError::InvalidInput(redact_secret(
            &err.to_string(),
            &token,
        ))),
    }
}

fn redact_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        return value.to_string();
    }
    value.replace(secret, "<redacted>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeDelta, Utc};
    use std::collections::HashMap;
    use std::sync::Arc;
    use uuid::Uuid;

    fn test_state() -> RuntimeState {
        RuntimeState::new(HashMap::new(), HashMap::new(), HashMap::new())
    }

    #[tokio::test]
    async fn cached_project_mcp_manager_is_reused_before_expiry() {
        let state = test_state();
        let project_id = Uuid::new_v4();
        let manager = Arc::new(McpAgentManager::from_tools_for_test(Vec::new()));
        state.project_mcp_managers.write().await.insert(
            project_id,
            ProjectMcpManagerHandle::with_token_expiry(
                Arc::clone(&manager),
                Utc::now() + TimeDelta::minutes(10),
            ),
        );

        let cached = cached_manager(&state, project_id).await.expect("cached");

        assert!(Arc::ptr_eq(&cached, &manager));
        assert!(
            state
                .project_mcp_managers
                .read()
                .await
                .contains_key(&project_id)
        );
    }

    #[tokio::test]
    async fn cached_project_mcp_manager_is_recreated_when_token_expires_soon() {
        let state = test_state();
        let project_id = Uuid::new_v4();
        state.project_mcp_managers.write().await.insert(
            project_id,
            ProjectMcpManagerHandle::with_token_expiry(
                Arc::new(McpAgentManager::from_tools_for_test(Vec::new())),
                Utc::now() + TimeDelta::seconds(60),
            ),
        );

        let cached = cached_manager(&state, project_id).await;

        assert!(cached.is_none());
        assert!(
            !state
                .project_mcp_managers
                .read()
                .await
                .contains_key(&project_id)
        );
    }

    #[tokio::test]
    async fn cached_project_mcp_manager_with_pat_token_is_reused_without_time_refresh() {
        let state = test_state();
        let project_id = Uuid::new_v4();
        let manager = Arc::new(McpAgentManager::from_tools_for_test(Vec::new()));
        state.project_mcp_managers.write().await.insert(
            project_id,
            ProjectMcpManagerHandle::without_token_expiry(Arc::clone(&manager)),
        );

        let cached = cached_manager(&state, project_id).await.expect("cached");

        assert!(Arc::ptr_eq(&cached, &manager));
    }
}
