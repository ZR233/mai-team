use std::fmt;
use std::sync::Arc;

use mai_protocol::AgentId;
use pl_core::{McpToolBackend, McpToolRequest, SecretRedaction};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::state::AgentRecord;
use crate::{AgentRuntime, RuntimeError};

#[derive(Clone)]
pub(crate) struct MaiMcpToolBackend {
    runtime: Arc<AgentRuntime>,
    agent: Arc<AgentRecord>,
    agent_id: AgentId,
    cancellation_token: CancellationToken,
}

impl MaiMcpToolBackend {
    pub(crate) fn new(
        runtime: Arc<AgentRuntime>,
        agent: Arc<AgentRecord>,
        agent_id: AgentId,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            runtime,
            agent,
            agent_id,
            cancellation_token,
        }
    }
}

impl fmt::Debug for MaiMcpToolBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MaiMcpToolBackend")
            .field("agent_id", &self.agent_id)
            .finish_non_exhaustive()
    }
}

impl McpToolBackend for MaiMcpToolBackend {
    type Error = String;

    async fn call_tool(&self, request: McpToolRequest) -> std::result::Result<Value, Self::Error> {
        if self.cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled.to_string());
        }
        self.call_tool_inner(request).await
    }
}

impl MaiMcpToolBackend {
    async fn call_tool_inner(&self, request: McpToolRequest) -> Result<Value, String> {
        if self.agent.summary.read().await.project_id.is_some() {
            return self.call_project_tool(request).await;
        }

        let manager = self.agent.mcp.read().await.clone().ok_or_else(|| {
            RuntimeError::InvalidInput("MCP manager not initialized".to_string()).to_string()
        })?;
        tokio::select! {
            output = manager.call_model_tool(&request.name, request.arguments) => {
                output.map_err(|error| error.to_string())
            }
            _ = self.cancellation_token.cancelled() => {
                Err(RuntimeError::TurnCancelled.to_string())
            }
        }
    }

    async fn call_project_tool(&self, request: McpToolRequest) -> Result<Value, String> {
        let tool = request.name.clone();
        let manager = self
            .runtime
            .project_mcp_manager_for_agent(&self.agent, self.agent_id, &self.cancellation_token)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                RuntimeError::InvalidInput("project MCP manager is not available".to_string())
                    .to_string()
            })?;
        let token = self
            .runtime
            .project_git_token_for_agent(&self.agent)
            .await
            .map_err(|error| error.to_string())?
            .unwrap_or_default();
        let redaction = SecretRedaction::new([token.as_str()]);
        let output = tokio::select! {
            output = manager.call_model_tool(&request.name, request.arguments) => output,
            _ = self.cancellation_token.cancelled() => {
                return Err(RuntimeError::TurnCancelled.to_string());
            }
        };
        match output {
            Ok(value) => Ok(redaction.redact_json_value(value)),
            Err(mai_mcp::McpError::ToolNotFound(_)) => Err(RuntimeError::InvalidInput(format!(
                "project MCP tool `{tool}` was not discovered"
            ))
            .to_string()),
            Err(error) => Err(redaction.redact_str(&error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mcp_tool_backend_delegates_tool_error_shape_to_pl_core() {
        let source = include_str!("mcp_tools.rs");

        assert!(
            !source.contains(&format!("{}{}", "ToolExecution", "Failed")),
            "MCP tool backend 不应在 mai-team 手动构造工具错误协议"
        );
        assert!(
            !source.contains(&format!("{}{}", "Pure", "Error")),
            "MCP tool backend 不应依赖 pl_protocol 错误类型"
        );
    }

    #[test]
    fn mcp_tool_backend_uses_pl_core_secret_redaction() {
        let source = include_str!("mcp_tools.rs");

        assert!(
            source.contains("SecretRedaction"),
            "MCP tool backend 应复用 pl-core 的 explicit secret redaction"
        );
        assert!(
            !source.contains(&format!("{}{}", "fn redact", "_value")),
            "MCP tool backend 不应保留本地 JSON 遮蔽实现"
        );
        assert!(
            !source.contains(&format!("{}{}", "fn redact", "_secret")),
            "MCP tool backend 不应保留本地 string 遮蔽实现"
        );
    }
}
