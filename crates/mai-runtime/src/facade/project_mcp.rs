use std::sync::Arc;

use crate::mcp::McpAgentManager;
use mai_protocol::{AgentId, ProjectId, ServiceEventKind};
use tokio_util::sync::CancellationToken;

use crate::state::AgentRecord;
use crate::turn::cancellation::ensure_not_cancelled;
use crate::{AgentRuntime, Result, RuntimeError, projects, redact_secret};

impl AgentRuntime {
    pub(crate) async fn ensure_project_mcp_manager(
        &self,
        project_id: ProjectId,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> Result<Option<Arc<McpAgentManager>>> {
        ensure_not_cancelled(cancellation_token)?;
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
}
