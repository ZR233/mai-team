use std::collections::HashMap;
use std::time::Duration;

use mai_protocol::McpServerConfig;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::{
    ClientInfo, ListResourceTemplatesResult, ListResourcesResult, ListToolsResult,
    ReadResourceRequestParams, ReadResourceResult,
};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::constants::DEFAULT_TOOL_TIMEOUT;
use crate::error::{McpError, Result};
use crate::protocol::{call_tool_params, client_info, paginated};
use crate::tools::parse_tools_result;
use crate::types::McpTool;

pub(crate) struct RmcpSession {
    server_name: String,
    config: McpServerConfig,
    service: Mutex<Option<RunningService<RoleClient, ClientInfo>>>,
}

impl RmcpSession {
    pub(crate) async fn start_http(server_name: String, config: McpServerConfig) -> Result<Self> {
        let url = config.url.as_deref().ok_or_else(|| {
            McpError::InvalidConfig(
                server_name.clone(),
                "streamable_http url is required".to_string(),
            )
        })?;
        if url.trim().is_empty() {
            return Err(McpError::InvalidConfig(
                server_name.clone(),
                "streamable_http url cannot be empty".to_string(),
            ));
        }
        let transport_config = streamable_http_config(&server_name, &config)?;
        let service = rmcp::serve_client(
            client_info(),
            StreamableHttpClientTransport::from_config(transport_config),
        )
        .await
        .map_err(|err| McpError::Server(server_name.clone(), err.to_string()))?;
        Ok(Self {
            server_name,
            config,
            service: Mutex::new(Some(service)),
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
    }

    fn tool_timeout(&self) -> Duration {
        self.config
            .tool_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TOOL_TIMEOUT)
    }
}

fn streamable_http_config(
    server_name: &str,
    config: &McpServerConfig,
) -> Result<StreamableHttpClientTransportConfig> {
    let url = config.url.clone().unwrap_or_default();
    let mut custom_headers = HashMap::new();
    for (key, value) in &config.headers {
        let name = HeaderName::from_bytes(key.as_bytes())
            .map_err(|err| McpError::Header(format!("{key}: {err}")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|err| McpError::Header(format!("{key}: {err}")))?;
        custom_headers.insert(name, value);
    }
    let bearer_token = config
        .bearer_token
        .clone()
        .or_else(|| {
            config
                .bearer_token_env
                .as_ref()
                .and_then(|key| std::env::var(key).ok())
        })
        .filter(|value| !value.trim().is_empty());
    if bearer_token.is_none() && config.bearer_token_env.is_some() {
        tracing::warn!(
            server = server_name,
            "MCP bearer_token_env is configured but the environment variable is not set"
        );
    }
    let mut out = StreamableHttpClientTransportConfig::with_uri(url);
    if let Some(token) = bearer_token {
        out = out.auth_header(token);
    }
    Ok(out.custom_headers(custom_headers))
}
