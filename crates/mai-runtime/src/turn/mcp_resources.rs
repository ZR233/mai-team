use std::sync::Arc;

use serde_json::Value;

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
    mcp: Option<pl_core::McpTurnLease>,
}

impl std::fmt::Debug for MaiMcpResourceBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaiMcpResourceBackend")
            .finish_non_exhaustive()
    }
}

impl MaiMcpResourceBackend {
    pub(crate) fn new(
        runtime: Arc<AgentRuntime>,
        agent: Arc<AgentRecord>,
        mcp: Option<pl_core::McpTurnLease>,
    ) -> Self {
        Self {
            runtime,
            agent,
            mcp,
        }
    }

    async fn broker(&self) -> crate::Result<crate::agents::AgentResourceBroker> {
        self.runtime.agent_resource_broker(&self.agent).await
    }

    fn is_skill_request(server: Option<&str>, uri: Option<&str>) -> bool {
        server.is_some_and(|server| {
            matches!(
                server,
                crate::agents::SKILL_RESOURCE_SERVER
                    | crate::agents::PROJECT_SKILL_RESOURCE_SERVER
                    | "mcp:skill"
                    | "mcp:project-skill"
            )
        }) || uri.is_some_and(|uri| uri.starts_with(crate::agents::SKILL_RESOURCE_SCHEME))
    }
}

impl pl_core::McpResourceBackend for MaiMcpResourceBackend {
    type Error = RuntimeError;

    async fn list_resources(
        &self,
        request: pl_core::McpListResourcesRequest,
    ) -> std::result::Result<Value, Self::Error> {
        if Self::is_skill_request(request.server.as_deref(), None) || self.mcp.is_none() {
            return self
                .broker()
                .await?
                .list_resources(request.server.as_deref(), request.cursor)
                .await;
        }
        if request.server.is_some() {
            return Ok(self
                .mcp
                .as_ref()
                .expect("checked above")
                .list_resources(request)
                .await?);
        }
        let skills = self.broker().await?.list_resources(None, None).await?;
        let mcp = self
            .mcp
            .as_ref()
            .expect("checked above")
            .list_resources(request)
            .await?;
        Ok(combine_resource_views(skills, mcp, "skills"))
    }

    async fn list_resource_templates(
        &self,
        request: pl_core::McpListResourceTemplatesRequest,
    ) -> std::result::Result<Value, Self::Error> {
        if Self::is_skill_request(request.server.as_deref(), None) || self.mcp.is_none() {
            return self
                .broker()
                .await?
                .list_resource_templates(request.server.as_deref(), request.cursor)
                .await;
        }
        if request.server.is_some() {
            return Ok(self
                .mcp
                .as_ref()
                .expect("checked above")
                .list_resource_templates(request)
                .await?);
        }
        let skills = self
            .broker()
            .await?
            .list_resource_templates(None, None)
            .await?;
        let mcp = self
            .mcp
            .as_ref()
            .expect("checked above")
            .list_resource_templates(request)
            .await?;
        Ok(combine_resource_views(skills, mcp, "skills"))
    }

    async fn read_resource(
        &self,
        request: pl_core::McpReadResourceRequest,
    ) -> std::result::Result<Value, Self::Error> {
        if Self::is_skill_request(Some(&request.server), Some(&request.uri)) || self.mcp.is_none() {
            return self
                .broker()
                .await?
                .read_resource(&request.server, &request.uri)
                .await;
        }
        Ok(self
            .mcp
            .as_ref()
            .expect("checked above")
            .read_resource(request)
            .await?)
    }
}

fn combine_resource_views(skills: Value, mcp: Value, skills_key: &str) -> Value {
    let mut values = match mcp {
        Value::Object(values) => values,
        value => serde_json::Map::from_iter([("mcp".to_string(), value)]),
    };
    values.insert(skills_key.to_string(), skills);
    Value::Object(values)
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
