use std::fmt;
use std::sync::Arc;

use mai_protocol::AgentId;
use pl_core::{McpToolBackend, McpToolRequest};
use pl_protocol::PureError;
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
    async fn call_tool(&self, request: McpToolRequest) -> pl_protocol::Result<Value> {
        if self.cancellation_token.is_cancelled() {
            return Err(PureError::ToolExecutionFailed {
                tool: request.name,
                error: RuntimeError::TurnCancelled.to_string(),
            });
        }
        self.call_tool_inner(request)
            .await
            .map_err(|error| PureError::ToolExecutionFailed {
                tool: error.0,
                error: error.1,
            })
    }
}

impl MaiMcpToolBackend {
    async fn call_tool_inner(&self, request: McpToolRequest) -> Result<Value, (String, String)> {
        if self.agent.summary.read().await.project_id.is_some() {
            return self.call_project_tool(request).await;
        }

        let manager = self.agent.mcp.read().await.clone().ok_or_else(|| {
            (
                request.name.clone(),
                RuntimeError::InvalidInput("MCP manager not initialized".to_string()).to_string(),
            )
        })?;
        let tool = request.name.clone();
        tokio::select! {
            output = manager.call_model_tool(&request.name, request.arguments) => {
                output.map_err(|error| (tool, error.to_string()))
            }
            _ = self.cancellation_token.cancelled() => {
                Err((tool, RuntimeError::TurnCancelled.to_string()))
            }
        }
    }

    async fn call_project_tool(&self, request: McpToolRequest) -> Result<Value, (String, String)> {
        let tool = request.name.clone();
        let manager = self
            .runtime
            .project_mcp_manager_for_agent(&self.agent, self.agent_id, &self.cancellation_token)
            .await
            .map_err(|error| (tool.clone(), error.to_string()))?
            .ok_or_else(|| {
                (
                    tool.clone(),
                    RuntimeError::InvalidInput("project MCP manager is not available".to_string())
                        .to_string(),
                )
            })?;
        let token = self
            .runtime
            .project_git_token_for_agent(&self.agent)
            .await
            .map_err(|error| (tool.clone(), error.to_string()))?
            .unwrap_or_default();
        let output = tokio::select! {
            output = manager.call_model_tool(&request.name, request.arguments) => output,
            _ = self.cancellation_token.cancelled() => {
                return Err((tool, RuntimeError::TurnCancelled.to_string()));
            }
        };
        match output {
            Ok(value) => Ok(redact_value(value, &token)),
            Err(mai_mcp::McpError::ToolNotFound(_)) => Err((
                tool.clone(),
                RuntimeError::InvalidInput(format!("project MCP tool `{tool}` was not discovered"))
                    .to_string(),
            )),
            Err(error) => Err((tool, redact_secret(&error.to_string(), &token))),
        }
    }
}

fn redact_value(value: Value, secret: &str) -> Value {
    if secret.is_empty() {
        return value;
    }
    let redacted = redact_secret(&value.to_string(), secret);
    serde_json::from_str(&redacted).unwrap_or(Value::String(redacted))
}

fn redact_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        return value.to_string();
    }
    value.replace(secret, "<redacted>")
}
