use std::sync::Arc;

use mai_protocol::AgentId;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::state::AgentRecord;
use crate::{AgentRuntime, RuntimeError};

/// mai-team 注入 pl-core MCP resource 工具的后端。
///
/// pl-core 负责共享工具 schema、输入解析、trace 和 tool result history；该类型只把
/// 当前 agent 可见的 MCP/skill resource broker 暴露给共享工具。
#[derive(Clone)]
pub(crate) struct MaiMcpResourceBackend {
    runtime: Arc<AgentRuntime>,
    agent: Arc<AgentRecord>,
    agent_id: AgentId,
    cancellation_token: CancellationToken,
}

impl std::fmt::Debug for MaiMcpResourceBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaiMcpResourceBackend")
            .field("agent_id", &self.agent_id)
            .finish_non_exhaustive()
    }
}

impl MaiMcpResourceBackend {
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

    async fn broker(&self) -> pl_protocol::Result<crate::agents::AgentResourceBroker> {
        self.runtime
            .agent_resource_broker(&self.agent, self.agent_id, &self.cancellation_token)
            .await
            .map_err(|error| resource_error("mcp_resource", error))
    }
}

impl pl_core::McpResourceBackend for MaiMcpResourceBackend {
    async fn list_resources(
        &self,
        request: pl_core::McpListResourcesRequest,
    ) -> pl_protocol::Result<Value> {
        self.broker()
            .await?
            .list_resources(request.server.as_deref(), request.cursor)
            .await
            .map_err(|error| resource_error(pl_core::TOOL_LIST_MCP_RESOURCES, error))
    }

    async fn list_resource_templates(
        &self,
        request: pl_core::McpListResourceTemplatesRequest,
    ) -> pl_protocol::Result<Value> {
        self.broker()
            .await?
            .list_resource_templates(request.server.as_deref(), request.cursor)
            .await
            .map_err(|error| resource_error(pl_core::TOOL_LIST_MCP_RESOURCE_TEMPLATES, error))
    }

    async fn read_resource(
        &self,
        request: pl_core::McpReadResourceRequest,
    ) -> pl_protocol::Result<Value> {
        self.broker()
            .await?
            .read_resource(&request.server, &request.uri)
            .await
            .map_err(|error| resource_error(pl_core::TOOL_READ_MCP_RESOURCE, error))
    }
}

fn resource_error(tool: &str, error: RuntimeError) -> pl_protocol::PureError {
    pl_protocol::PureError::ToolExecutionFailed {
        tool: tool.to_string(),
        error: error.to_string(),
    }
}
