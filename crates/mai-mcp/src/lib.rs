use futures::{StreamExt, stream};
use mai_docker::{DockerClient, DockerError};
use mai_protocol::{McpServerConfig, McpServerTransport, McpStartupStatus};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation,
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ReadResourceRequestParams, ReadResourceResult, Tool,
};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Error)]
pub enum McpError {
    #[error("docker error: {0}")]
    Docker(#[from] DockerError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("header error: {0}")]
    Header(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("mcp service error: {0}")]
    Service(#[from] ServiceError),
    #[error("mcp server `{0}` failed: {1}")]
    Server(String, String),
    #[error("mcp tool `{0}` not found")]
    ToolNotFound(String),
    #[error("mcp session missing stdio")]
    MissingStdio,
    #[error("mcp server `{0}` timed out during {1}")]
    Timeout(String, String),
    #[error("mcp server `{0}` has invalid config: {1}")]
    InvalidConfig(String, String),
}

pub type Result<T> = std::result::Result<T, McpError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    pub model_name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub output_schema: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerStatus {
    pub server: String,
    pub status: McpStartupStatus,
    pub error: Option<String>,
    pub required: bool,
}

pub struct McpAgentManager {
    sessions: RwLock<BTreeMap<String, Arc<McpSession>>>,
    tools: RwLock<BTreeMap<String, McpTool>>,
    statuses: RwLock<BTreeMap<String, McpServerStatus>>,
}

impl McpAgentManager {
    pub async fn start(
        docker: DockerClient,
        container_id: String,
        configs: BTreeMap<String, McpServerConfig>,
    ) -> Self {
        let manager = Self {
            sessions: RwLock::new(BTreeMap::new()),
            tools: RwLock::new(BTreeMap::new()),
            statuses: RwLock::new(BTreeMap::new()),
        };

        let enabled_configs = configs
            .into_iter()
            .filter(|(_, config)| config.enabled)
            .collect::<Vec<_>>();
        for (server_name, config) in &enabled_configs {
            manager
                .set_status(
                    server_name,
                    McpStartupStatus::Starting,
                    None,
                    config.required,
                )
                .await;
        }

        let mut startup_tasks =
            stream::iter(enabled_configs.into_iter().map(|(server_name, config)| {
                let docker = docker.clone();
                let container_id = container_id.clone();
                let required = config.required;
                async move {
                    let result =
                        start_server_session(docker, container_id, server_name.clone(), config)
                            .await;
                    (server_name, required, result)
                }
            }))
            .buffer_unordered(16);

        while let Some((server_name, required, result)) = startup_tasks.next().await {
            match result {
                Ok((session, tools)) => {
                    manager
                        .sessions
                        .write()
                        .await
                        .insert(server_name.clone(), Arc::clone(&session));
                    let mut tool_map = manager.tools.write().await;
                    for mut tool in tools {
                        tool.model_name = collision_safe_tool_name(&tool_map, &tool);
                        tool_map.insert(tool.model_name.clone(), tool);
                    }
                    manager
                        .set_status(&server_name, McpStartupStatus::Ready, None, required)
                        .await;
                }
                Err(err) => {
                    manager
                        .set_status(
                            &server_name,
                            McpStartupStatus::Failed,
                            Some(err.to_string()),
                            required,
                        )
                        .await;
                    tracing::warn!("failed to initialize MCP server `{server_name}`: {err}");
                }
            }
        }

        manager
    }

    pub async fn shutdown(&self) {
        let sessions = std::mem::take(&mut *self.sessions.write().await);
        for session in sessions.into_values() {
            session.shutdown().await;
        }
    }

    pub async fn statuses(&self) -> Vec<McpServerStatus> {
        self.statuses.read().await.values().cloned().collect()
    }

    pub async fn required_failures(&self) -> Vec<McpServerStatus> {
        self.statuses
            .read()
            .await
            .values()
            .filter(|status| status.required && status.status == McpStartupStatus::Failed)
            .cloned()
            .collect()
    }

    pub async fn tools(&self) -> Vec<McpTool> {
        self.tools.read().await.values().cloned().collect()
    }

    pub async fn call_model_tool(&self, model_name: &str, arguments: Value) -> Result<Value> {
        let tool = self
            .tools
            .read()
            .await
            .get(model_name)
            .cloned()
            .ok_or_else(|| McpError::ToolNotFound(model_name.to_string()))?;
        let session = self
            .session(&tool.server)
            .await
            .map_err(|err| McpError::Server(tool.server.clone(), err.to_string()))?;
        session.call_tool(&tool.name, arguments).await
    }

    pub async fn list_resources(
        &self,
        server: Option<&str>,
        cursor: Option<String>,
    ) -> Result<Value> {
        if let Some(server) = server {
            let result = self.session(server).await?.list_resources(cursor).await?;
            return Ok(list_resources_value(Some(server), result));
        }
        let mut resources = Vec::new();
        for (server, session) in self.sessions.read().await.iter() {
            let result = session.list_resources(None).await?;
            for resource in result.resources {
                resources.push(with_server(server, serde_json::to_value(resource)?));
            }
        }
        Ok(json!({ "resources": resources }))
    }

    pub async fn list_resource_templates(
        &self,
        server: Option<&str>,
        cursor: Option<String>,
    ) -> Result<Value> {
        if let Some(server) = server {
            let result = self
                .session(server)
                .await?
                .list_resource_templates(cursor)
                .await?;
            return Ok(list_resource_templates_value(Some(server), result));
        }
        let mut templates = Vec::new();
        for (server, session) in self.sessions.read().await.iter() {
            let result = session.list_resource_templates(None).await?;
            for template in result.resource_templates {
                templates.push(with_server(server, serde_json::to_value(template)?));
            }
        }
        Ok(json!({ "resourceTemplates": templates }))
    }

    pub async fn read_resource(&self, server: &str, uri: &str) -> Result<Value> {
        Ok(serde_json::to_value(
            self.session(server).await?.read_resource(uri).await?,
        )?)
    }

    async fn session(&self, server: &str) -> Result<Arc<McpSession>> {
        self.sessions
            .read()
            .await
            .get(server)
            .cloned()
            .ok_or_else(|| McpError::Server(server.to_string(), "session not found".to_string()))
    }

    async fn set_status(
        &self,
        server: &str,
        status: McpStartupStatus,
        error: Option<String>,
        required: bool,
    ) {
        self.statuses.write().await.insert(
            server.to_string(),
            McpServerStatus {
                server: server.to_string(),
                status,
                error,
                required,
            },
        );
    }
}

async fn start_server_session(
    docker: DockerClient,
    container_id: String,
    server_name: String,
    config: McpServerConfig,
) -> Result<(Arc<McpSession>, Vec<McpTool>)> {
    let startup_timeout = config
        .startup_timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_STARTUP_TIMEOUT);
    let timeout_server = server_name.clone();
    timeout(startup_timeout, async move {
        let session =
            Arc::new(McpSession::start(&docker, &container_id, server_name, config).await?);
        let tools = session.list_tools().await?;
        Ok((session, tools))
    })
    .await
    .map_err(|_| McpError::Timeout(timeout_server, "initialize".to_string()))?
}

enum McpSession {
    Stdio(StdioMcpSession),
    Http(RmcpSession),
}

impl McpSession {
    async fn start(
        docker: &DockerClient,
        container_id: &str,
        server_name: String,
        config: McpServerConfig,
    ) -> Result<Self> {
        match config.transport {
            McpServerTransport::Stdio => Ok(Self::Stdio(
                StdioMcpSession::start(docker, container_id, server_name, config).await?,
            )),
            McpServerTransport::StreamableHttp => Ok(Self::Http(
                RmcpSession::start_http(server_name, config).await?,
            )),
        }
    }

    async fn list_tools(&self) -> Result<Vec<McpTool>> {
        match self {
            Self::Stdio(session) => session.list_tools().await,
            Self::Http(session) => session.list_tools().await,
        }
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        match self {
            Self::Stdio(session) => session.call_tool(name, arguments).await,
            Self::Http(session) => session.call_tool(name, arguments).await,
        }
    }

    async fn list_resources(&self, cursor: Option<String>) -> Result<ListResourcesResult> {
        match self {
            Self::Stdio(session) => session.list_resources(cursor).await,
            Self::Http(session) => session.list_resources(cursor).await,
        }
    }

    async fn list_resource_templates(
        &self,
        cursor: Option<String>,
    ) -> Result<ListResourceTemplatesResult> {
        match self {
            Self::Stdio(session) => session.list_resource_templates(cursor).await,
            Self::Http(session) => session.list_resource_templates(cursor).await,
        }
    }

    async fn read_resource(&self, uri: &str) -> Result<ReadResourceResult> {
        match self {
            Self::Stdio(session) => session.read_resource(uri).await,
            Self::Http(session) => session.read_resource(uri).await,
        }
    }

    async fn shutdown(&self) {
        match self {
            Self::Stdio(session) => session.shutdown().await,
            Self::Http(session) => session.shutdown().await,
        }
    }
}

struct StdioMcpSession {
    server_name: String,
    config: McpServerConfig,
    service: Mutex<Option<RunningService<RoleClient, ClientInfo>>>,
    child: Mutex<Child>,
}

impl StdioMcpSession {
    async fn start(
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

    async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let result = self.list_tools_result().await?;
        Ok(parse_tools_result(&self.server_name, &self.config, result))
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let params = call_tool_params(name, arguments)?;
        let label = format!("tools/call {name}");
        let guard = self.service.lock().await;
        let service = self.service_ref(&guard)?;
        let result = timeout(self.tool_timeout(), service.peer().call_tool(params))
            .await
            .map_err(|_| McpError::Timeout(self.server_name.clone(), label))??;
        Ok(serde_json::to_value(result)?)
    }

    async fn list_resources(&self, cursor: Option<String>) -> Result<ListResourcesResult> {
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

    async fn list_resource_templates(
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

    async fn read_resource(&self, uri: &str) -> Result<ReadResourceResult> {
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

    async fn shutdown(&self) {
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

struct RmcpSession {
    server_name: String,
    config: McpServerConfig,
    service: Mutex<Option<RunningService<RoleClient, ClientInfo>>>,
}

impl RmcpSession {
    async fn start_http(server_name: String, config: McpServerConfig) -> Result<Self> {
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

    async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let result = self.list_tools_result().await?;
        Ok(parse_tools_result(&self.server_name, &self.config, result))
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let params = call_tool_params(name, arguments)?;
        let label = format!("tools/call {name}");
        let guard = self.service.lock().await;
        let service = self.service_ref(&guard)?;
        let result = timeout(self.tool_timeout(), service.peer().call_tool(params))
            .await
            .map_err(|_| McpError::Timeout(self.server_name.clone(), label))??;
        Ok(serde_json::to_value(result)?)
    }

    async fn list_resources(&self, cursor: Option<String>) -> Result<ListResourcesResult> {
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

    async fn list_resource_templates(
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

    async fn read_resource(&self, uri: &str) -> Result<ReadResourceResult> {
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

    async fn shutdown(&self) {
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

fn rmcp_transport(
    stdout: ChildStdout,
    stdin: ChildStdin,
) -> AsyncRwTransport<RoleClient, ChildStdout, ChildStdin> {
    AsyncRwTransport::new(stdout, stdin)
}

fn client_info() -> ClientInfo {
    ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("mai-team", env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(ProtocolVersion::V_2025_06_18)
}

fn parse_tools_result(
    server: &str,
    config: &McpServerConfig,
    result: ListToolsResult,
) -> Vec<McpTool> {
    let enabled = config
        .enabled_tools
        .as_ref()
        .map(|tools| tools.iter().cloned().collect::<BTreeSet<_>>());
    let disabled = config
        .disabled_tools
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    result
        .tools
        .into_iter()
        .filter(|tool| {
            enabled
                .as_ref()
                .is_none_or(|tools| tools.contains(tool.name.as_ref()))
                && !disabled.contains(tool.name.as_ref())
        })
        .map(|tool| parse_tool(server, tool))
        .collect()
}

fn parse_tool(server: &str, tool: Tool) -> McpTool {
    let name = tool.name.to_string();
    let description = tool.description.unwrap_or_default().to_string();
    let input_schema = normalize_input_schema(Value::Object(tool.input_schema.as_ref().clone()));
    let output_schema = tool
        .output_schema
        .map(|schema| Value::Object(schema.as_ref().clone()));
    McpTool {
        model_name: model_tool_name(server, &name),
        server: server.to_string(),
        name,
        description,
        input_schema,
        output_schema,
    }
}

fn normalize_input_schema(mut schema: Value) -> Value {
    if !schema.is_object() {
        schema = json!({ "type": "object", "properties": {} });
    }
    if let Value::Object(map) = &mut schema {
        map.entry("type".to_string())
            .or_insert_with(|| Value::String("object".to_string()));
        let missing_properties = map
            .get("properties")
            .is_none_or(|properties| properties.is_null());
        if missing_properties {
            map.insert("properties".to_string(), Value::Object(Map::new()));
        }
    }
    schema
}

fn call_tool_params(name: &str, arguments: Value) -> Result<CallToolRequestParams> {
    let arguments = match arguments {
        Value::Object(map) => Some(map),
        Value::Null => None,
        other => {
            return Err(McpError::InvalidConfig(
                "tool".to_string(),
                format!("MCP tool arguments must be a JSON object, got {other}"),
            ));
        }
    };
    let mut params = CallToolRequestParams::new(name.to_string());
    params.arguments = arguments;
    Ok(params)
}

fn paginated(cursor: Option<String>) -> Option<PaginatedRequestParams> {
    Some(PaginatedRequestParams::default().with_cursor(cursor))
}

fn list_resources_value(server: Option<&str>, result: ListResourcesResult) -> Value {
    let resources = result
        .resources
        .into_iter()
        .map(|resource| {
            let value = serde_json::to_value(resource).unwrap_or(Value::Null);
            match server {
                Some(server) => with_server(server, value),
                None => value,
            }
        })
        .collect::<Vec<_>>();
    json!({
        "server": server,
        "resources": resources,
        "nextCursor": result.next_cursor,
    })
}

fn list_resource_templates_value(
    server: Option<&str>,
    result: ListResourceTemplatesResult,
) -> Value {
    let resource_templates = result
        .resource_templates
        .into_iter()
        .map(|template| {
            let value = serde_json::to_value(template).unwrap_or(Value::Null);
            match server {
                Some(server) => with_server(server, value),
                None => value,
            }
        })
        .collect::<Vec<_>>();
    json!({
        "server": server,
        "resourceTemplates": resource_templates,
        "nextCursor": result.next_cursor,
    })
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

fn with_server(server: &str, value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            map.insert("server".to_string(), Value::String(server.to_string()));
            Value::Object(map)
        }
        other => json!({ "server": server, "value": other }),
    }
}

fn collision_safe_tool_name(existing: &BTreeMap<String, McpTool>, tool: &McpTool) -> String {
    if !existing.contains_key(&tool.model_name) {
        return tool.model_name.clone();
    }
    let suffix = fnv1a_hex(&format!("{}::{}", tool.server, tool.name));
    let keep = 64usize.saturating_sub(suffix.len() + 2);
    let prefix = if tool.model_name.len() > keep {
        &tool.model_name[..keep]
    } else {
        &tool.model_name
    };
    let mut candidate = format!("{prefix}__{suffix}");
    let mut index = 2usize;
    while existing.contains_key(&candidate) {
        let extra = format!("_{index}");
        let keep = 64usize.saturating_sub(suffix.len() + extra.len() + 2);
        let prefix = if tool.model_name.len() > keep {
            &tool.model_name[..keep]
        } else {
            &tool.model_name
        };
        candidate = format!("{prefix}__{suffix}{extra}");
        index += 1;
    }
    candidate
}

pub fn model_tool_name(server: &str, tool: &str) -> String {
    let base = format!("mcp__{}__{}", sanitize_name(server), sanitize_name(tool));
    if base.len() <= 64 {
        return base;
    }
    let hash = fnv1a_hex(&base);
    let keep = 64usize.saturating_sub(hash.len() + 2);
    format!("{}__{}", &base[..keep], hash)
}

fn sanitize_name(value: &str) -> String {
    let mut out = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if out.is_empty() {
        out = "tool".to_string();
    }
    if out.as_bytes()[0].is_ascii_digit() {
        out.insert(0, '_');
    }
    out
}

fn fnv1a_hex(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn model_tool_names_are_sanitized() {
        assert_eq!(
            model_tool_name("fs.server", "read file"),
            "mcp__fs_server__read_file"
        );
        assert!(model_tool_name("1", "2").starts_with("mcp___1___2"));
    }

    #[test]
    fn long_model_tool_names_are_limited() {
        let name = model_tool_name(&"a".repeat(80), &"b".repeat(80));
        assert!(name.len() <= 64);
    }

    #[test]
    fn tool_schema_gets_properties() {
        let tool = Tool::new_with_raw(
            "echo",
            None,
            Map::from_iter([("type".to_string(), json!("object"))]),
        );
        let tool = parse_tool("demo", tool);
        assert!(tool.input_schema.get("properties").is_some());
    }

    #[test]
    fn parse_tools_applies_allow_and_deny_filters() {
        let config = McpServerConfig {
            enabled_tools: Some(vec!["keep".to_string(), "drop".to_string()]),
            disabled_tools: vec!["drop".to_string()],
            ..Default::default()
        };
        let result = ListToolsResult {
            tools: vec![
                Tool::new_with_raw("keep", Some(Cow::Borrowed("")), Map::new()),
                Tool::new_with_raw("drop", Some(Cow::Borrowed("")), Map::new()),
                Tool::new_with_raw("other", Some(Cow::Borrowed("")), Map::new()),
            ],
            ..Default::default()
        };

        let tools = parse_tools_result("demo", &config, result);

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "keep");
    }

    #[test]
    fn collision_safe_tool_names_preserve_both_tools() {
        let first = McpTool {
            server: "a.b".to_string(),
            name: "read file".to_string(),
            model_name: model_tool_name("a.b", "read file"),
            description: String::new(),
            input_schema: json!({ "type": "object", "properties": {} }),
            output_schema: None,
        };
        let second = McpTool {
            server: "a_b".to_string(),
            name: "read_file".to_string(),
            model_name: model_tool_name("a_b", "read_file"),
            description: String::new(),
            input_schema: json!({ "type": "object", "properties": {} }),
            output_schema: None,
        };
        let mut existing = BTreeMap::new();
        existing.insert(first.model_name.clone(), first);

        let name = collision_safe_tool_name(&existing, &second);

        assert_ne!(name, second.model_name);
        assert!(name.starts_with("mcp__a_b__read_file__"));
        assert!(name.len() <= 64);
    }
}
