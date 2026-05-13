use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mai_docker::{DockerClient, SidecarParams};
use mai_protocol::McpServerConfig;
use rmcp::model::{
    ClientInfo, ListResourceTemplatesResult, ListResourcesResult, ListToolsResult,
    ReadResourceRequestParams, ReadResourceResult,
};
use rmcp::service::{RoleClient, RunningService};
use serde_json::Value;
use tokio::process::Child;
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::constants::DEFAULT_TOOL_TIMEOUT;
use crate::error::{McpError, Result};
use crate::naming::sanitize_name;
use crate::protocol::{call_tool_params, client_info, paginated, rmcp_transport};
use crate::tools::parse_tools_result;
use crate::types::McpTool;

pub(crate) struct StdioMcpSession {
    server_name: String,
    config: McpServerConfig,
    service: Mutex<Option<RunningService<RoleClient, ClientInfo>>>,
    child: Mutex<Child>,
}

impl StdioMcpSession {
    pub(crate) async fn start(
        docker: &DockerClient,
        container_id: &str,
        server_name: String,
        config: McpServerConfig,
    ) -> Result<Self> {
        let command = config.command.as_deref().ok_or_else(|| {
            McpError::InvalidConfig(server_name.clone(), "stdio command is required".to_string())
        })?;
        let env = config
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        let mut child = docker.spawn_exec(
            container_id,
            command,
            &config.args,
            config.cwd.as_deref(),
            &env,
        )?;
        let stdin = child.stdin.take().ok_or(McpError::MissingStdio)?;
        let stdout = child.stdout.take().ok_or(McpError::MissingStdio)?;
        let service = rmcp::serve_client(client_info(), rmcp_transport(stdout, stdin))
            .await
            .map_err(|err| McpError::Server(server_name.clone(), err.to_string()))?;
        Ok(Self {
            server_name,
            config,
            service: Mutex::new(Some(service)),
            child: Mutex::new(child),
        })
    }

    pub(crate) async fn start_sidecar(
        docker: &DockerClient,
        workspace_volume: &str,
        image: &str,
        server_name: String,
        config: McpServerConfig,
    ) -> Result<Self> {
        let command = config.command.as_deref().ok_or_else(|| {
            McpError::InvalidConfig(server_name.clone(), "stdio command is required".to_string())
        })?;
        let env = config
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default();
        let name = format!(
            "mai-team-mcp-{}-{}-{nonce}",
            sanitize_name(&server_name),
            std::process::id()
        );
        let mut child = docker.spawn_sidecar(&SidecarParams {
            name: &name,
            image,
            command,
            args: &config.args,
            cwd: config.cwd.as_deref(),
            env: &env,
            workspace_volume: Some(workspace_volume),
            timeout_secs: None,
        })?;
        let stdin = child.stdin.take().ok_or(McpError::MissingStdio)?;
        let stdout = child.stdout.take().ok_or(McpError::MissingStdio)?;
        let service = rmcp::serve_client(client_info(), rmcp_transport(stdout, stdin))
            .await
            .map_err(|err| McpError::Server(server_name.clone(), err.to_string()))?;
        Ok(Self {
            server_name,
            config,
            service: Mutex::new(Some(service)),
            child: Mutex::new(child),
        })
    }

    pub(crate) async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let result = self.list_tools_result().await?;
        Ok(parse_tools_result(&self.server_name, &self.config, result))
    }

    pub(crate) async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let params = call_tool_params(name, arguments)?;
        let label = format!("tools/call {name}");
        let guard = self.service.lock().await;
        let service = self.service_ref(&guard)?;
        let result = timeout(self.tool_timeout(), service.peer().call_tool(params))
            .await
            .map_err(|_| McpError::Timeout(self.server_name.clone(), label))??;
        Ok(serde_json::to_value(result)?)
    }

    pub(crate) async fn list_resources(
        &self,
        cursor: Option<String>,
    ) -> Result<ListResourcesResult> {
        let guard = self.service.lock().await;
        let service = self.service_ref(&guard)?;
        timeout(
            self.tool_timeout(),
            service.peer().list_resources(paginated(cursor)),
        )
        .await
        .map_err(|_| McpError::Timeout(self.server_name.clone(), "resources/list".to_string()))?
        .map_err(McpError::from)
    }

    pub(crate) async fn list_resource_templates(
        &self,
        cursor: Option<String>,
    ) -> Result<ListResourceTemplatesResult> {
        let guard = self.service.lock().await;
        let service = self.service_ref(&guard)?;
        timeout(
            self.tool_timeout(),
            service.peer().list_resource_templates(paginated(cursor)),
        )
        .await
        .map_err(|_| {
            McpError::Timeout(
                self.server_name.clone(),
                "resources/templates/list".to_string(),
            )
        })?
        .map_err(McpError::from)
    }

    pub(crate) async fn read_resource(&self, uri: &str) -> Result<ReadResourceResult> {
        let params = ReadResourceRequestParams::new(uri.to_string());
        let guard = self.service.lock().await;
        let service = self.service_ref(&guard)?;
        timeout(self.tool_timeout(), service.peer().read_resource(params))
            .await
            .map_err(|_| McpError::Timeout(self.server_name.clone(), "resources/read".to_string()))?
            .map_err(McpError::from)
    }

    async fn list_tools_result(&self) -> Result<ListToolsResult> {
        let guard = self.service.lock().await;
        let service = self.service_ref(&guard)?;
        timeout(self.tool_timeout(), service.peer().list_tools(None))
            .await
            .map_err(|_| McpError::Timeout(self.server_name.clone(), "tools/list".to_string()))?
            .map_err(McpError::from)
    }

    fn service_ref<'a>(
        &self,
        guard: &'a Option<RunningService<RoleClient, ClientInfo>>,
    ) -> Result<&'a RunningService<RoleClient, ClientInfo>> {
        guard
            .as_ref()
            .ok_or_else(|| McpError::Server(self.server_name.clone(), "session closed".to_string()))
    }

    pub(crate) async fn shutdown(&self) {
        if let Some(mut service) = self.service.lock().await.take() {
            let _ = service.close_with_timeout(Duration::from_secs(2)).await;
        }
        let mut child = self.child.lock().await;
        if let Err(err) = child.kill().await {
            tracing::warn!(
                "failed to kill MCP stdio server `{}`: {err}",
                self.server_name
            );
        }
    }

    fn tool_timeout(&self) -> Duration {
        self.config
            .tool_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TOOL_TIMEOUT)
    }
}
