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

    async fn broker(&self) -> crate::Result<crate::agents::AgentResourceBroker> {
        self.runtime
            .agent_resource_broker(&self.agent, self.agent_id, &self.cancellation_token)
            .await
    }
}

impl pl_core::McpResourceBackend for MaiMcpResourceBackend {
    type Error = RuntimeError;

    async fn list_resources(
        &self,
        request: pl_core::McpListResourcesRequest,
    ) -> std::result::Result<Value, Self::Error> {
        self.broker()
            .await?
            .list_resources(request.server.as_deref(), request.cursor)
            .await
    }

    async fn list_resource_templates(
        &self,
        request: pl_core::McpListResourceTemplatesRequest,
    ) -> std::result::Result<Value, Self::Error> {
        self.broker()
            .await?
            .list_resource_templates(request.server.as_deref(), request.cursor)
            .await
    }

    async fn read_resource(
        &self,
        request: pl_core::McpReadResourceRequest,
    ) -> std::result::Result<Value, Self::Error> {
        self.broker()
            .await?
            .read_resource(&request.server, &request.uri)
            .await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mcp_resource_backend_delegates_tool_error_shape_to_pl_core() {
        let source = include_str!("mcp_resources.rs");

        assert!(
            !source.contains(&format!("{}{}", "ToolExecution", "Failed")),
            "MCP resource backend 不应在 mai-team 手动构造工具错误协议"
        );
        assert!(
            !source.contains(&format!("{}{}", "Pure", "Error")),
            "MCP resource backend 不应依赖 pl_protocol 错误类型"
        );
    }
}
