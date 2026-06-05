use std::sync::Arc;

use mai_mcp::McpAgentManager;
use mai_protocol::{AgentId, ProjectId, ServiceEventKind};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::state::AgentRecord;
use crate::turn::tools::ToolExecution;
use crate::{AgentRuntime, Result, RuntimeError, projects, redact_secret};

impl AgentRuntime {
    pub(crate) async fn ensure_project_mcp_manager(
        &self,
        project_id: ProjectId,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> Result<Option<Arc<McpAgentManager>>> {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        if projects::mcp::project_mcp_configs("").is_empty() {
            return Ok(None);
        }
        if let Some(manager) = projects::mcp::cached_manager(&self.state, project_id).await {
            return Ok(Some(manager));
        }

        let Some(token) = self.project_git_token_details(project_id).await? else {
            return Ok(None);
        };
        let token_secret = token.token.clone();
        self.events
            .publish(ServiceEventKind::McpServerStatusChanged {
                agent_id,
                server: "project".to_string(),
                status: mai_protocol::McpStartupStatus::Starting,
                error: None,
            })
            .await;
        let manager = projects::mcp::ensure_manager(
            &self.state,
            &self.deps.docker,
            &self.sidecar_image,
            project_id,
            projects::mcp::ProjectMcpCredential {
                token: token.token,
                expires_at: token.expires_at,
            },
            cancellation_token,
        )
        .await
        .map_err(|err| match err {
            RuntimeError::TurnCancelled => RuntimeError::TurnCancelled,
            other => RuntimeError::InvalidInput(redact_secret(&other.to_string(), &token_secret)),
        })?;
        for status in manager.statuses().await {
            let error = status
                .error
                .map(|error| redact_secret(&error, &token_secret));
            self.events
                .publish(ServiceEventKind::McpServerStatusChanged {
                    agent_id,
                    server: status.server,
                    status: status.status,
                    error,
                })
                .await;
        }
        Ok(Some(manager))
    }

    pub(crate) async fn project_mcp_manager_for_agent(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> Result<Option<Arc<McpAgentManager>>> {
        let Some(project_id) = agent.summary.read().await.project_id else {
            return Ok(None);
        };
        self.ensure_project_mcp_manager(project_id, agent_id, cancellation_token)
            .await
    }

    pub(crate) async fn shutdown_project_mcp_manager(&self, project_id: ProjectId) {
        projects::mcp::shutdown_manager(&self.state, project_id).await;
    }

    pub(crate) async fn delete_project_sidecar(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<String>> {
        projects::mcp::delete_sidecar(&self.state, &self.deps.docker, project_id).await
    }

    pub(crate) async fn execute_project_mcp_tool(
        &self,
        agent: &AgentRecord,
        model_name: &str,
        arguments: Value,
        cancellation_token: CancellationToken,
    ) -> Result<ToolExecution> {
        projects::mcp::execute_project_mcp_tool(
            self,
            agent,
            model_name,
            arguments,
            cancellation_token,
        )
        .await
    }
}

impl projects::mcp::ProjectMcpToolOps for AgentRuntime {
    fn project_mcp_manager_for_agent(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> impl std::future::Future<Output = Result<Option<Arc<McpAgentManager>>>> + Send {
        AgentRuntime::project_mcp_manager_for_agent(self, agent, agent_id, cancellation_token)
    }

    fn project_git_token_for_agent(
        &self,
        agent: &AgentRecord,
    ) -> impl std::future::Future<Output = Result<Option<String>>> + Send {
        AgentRuntime::project_git_token_for_agent(self, agent)
    }

    async fn call_project_mcp_tool(
        &self,
        manager: Arc<McpAgentManager>,
        model_name: String,
        arguments: Value,
    ) -> std::result::Result<Value, mai_mcp::McpError> {
        manager.call_model_tool(&model_name, arguments).await
    }
}
