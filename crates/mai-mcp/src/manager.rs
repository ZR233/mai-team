use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::{StreamExt, stream};
use mai_docker::DockerClient;
use mai_protocol::{McpServerConfig, McpStartupStatus};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio::time::timeout;

use crate::constants::DEFAULT_STARTUP_TIMEOUT;
use crate::error::{McpError, Result};
use crate::resources::{list_resource_templates_value, list_resources_value, with_server};
use crate::session::McpSession;
use crate::tools::collision_safe_tool_name;
use crate::types::{McpServerStatus, McpTool};

pub struct McpAgentManager {
    sessions: RwLock<BTreeMap<String, Arc<McpSession>>>,
    tools: RwLock<BTreeMap<String, McpTool>>,
    statuses: RwLock<BTreeMap<String, McpServerStatus>>,
    #[cfg(debug_assertions)]
    test_resources: RwLock<BTreeMap<String, Vec<Value>>>,
}

impl McpAgentManager {
    #[doc(hidden)]
    pub fn from_tools_for_test(tools: Vec<McpTool>) -> Self {
        Self {
            sessions: RwLock::new(BTreeMap::new()),
            tools: RwLock::new(
                tools
                    .into_iter()
                    .map(|tool| (tool.model_name.clone(), tool))
                    .collect(),
            ),
            statuses: RwLock::new(BTreeMap::new()),
            #[cfg(debug_assertions)]
            test_resources: RwLock::new(BTreeMap::new()),
        }
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn from_resources_for_test(resources: Vec<(&str, Vec<Value>)>) -> Self {
        Self {
            sessions: RwLock::new(BTreeMap::new()),
            tools: RwLock::new(BTreeMap::new()),
            statuses: RwLock::new(BTreeMap::new()),
            test_resources: RwLock::new(
                resources
                    .into_iter()
                    .map(|(server, resources)| (server.to_string(), resources))
                    .collect(),
            ),
        }
    }

    pub async fn start(
        docker: DockerClient,
        container_id: String,
        configs: BTreeMap<String, McpServerConfig>,
    ) -> Self {
        let manager = Self::empty();
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

    pub async fn start_sidecars(
        docker: DockerClient,
        workspace_volume: String,
        image: String,
        configs: BTreeMap<String, McpServerConfig>,
    ) -> Self {
        let manager = Self::empty();
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
                let workspace_volume = workspace_volume.clone();
                let image = image.clone();
                async move {
                    let required = config.required;
                    let result = start_sidecar_server_session(
                        docker,
                        workspace_volume,
                        image,
                        server_name.clone(),
                        config,
                    )
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
                    tracing::warn!("failed to initialize MCP sidecar `{server_name}`: {err}");
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

    pub async fn resource_servers(&self) -> Vec<String> {
        let mut servers = self
            .sessions
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        #[cfg(debug_assertions)]
        {
            for server in self.test_resources.read().await.keys() {
                if !servers.iter().any(|existing| existing == server) {
                    servers.push(server.clone());
                }
            }
        }
        servers
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
        #[cfg(debug_assertions)]
        if let Some(value) = self.test_list_resources(server).await {
            return Ok(value);
        }
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
        #[cfg(debug_assertions)]
        if let Some(value) = self.test_read_resource(server, uri).await {
            return Ok(value);
        }
        Ok(serde_json::to_value(
            self.session(server).await?.read_resource(uri).await?,
        )?)
    }

    fn empty() -> Self {
        Self {
            sessions: RwLock::new(BTreeMap::new()),
            tools: RwLock::new(BTreeMap::new()),
            statuses: RwLock::new(BTreeMap::new()),
            #[cfg(debug_assertions)]
            test_resources: RwLock::new(BTreeMap::new()),
        }
    }

    #[cfg(debug_assertions)]
    async fn test_list_resources(&self, server: Option<&str>) -> Option<Value> {
        let resources = self.test_resources.read().await;
        if resources.is_empty() {
            return None;
        }
        if let Some(server) = server {
            return resources.get(server).map(|items| {
                json!({
                    "server": server,
                    "resources": items,
                    "nextCursor": null,
                })
            });
        }
        let all = resources
            .iter()
            .flat_map(|(server, items)| items.iter().map(|item| with_server(server, item.clone())))
            .collect::<Vec<_>>();
        Some(json!({ "resources": all }))
    }

    #[cfg(debug_assertions)]
    async fn test_read_resource(&self, server: &str, uri: &str) -> Option<Value> {
        let resources = self.test_resources.read().await;
        let item = resources
            .get(server)?
            .iter()
            .find(|item| item.get("uri").and_then(Value::as_str) == Some(uri))?;
        Some(json!({
            "contents": [{
                "uri": uri,
                "mimeType": item
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("application/json"),
                "text": item.to_string(),
            }]
        }))
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

async fn start_sidecar_server_session(
    docker: DockerClient,
    workspace_volume: String,
    image: String,
    server_name: String,
    config: McpServerConfig,
) -> Result<(Arc<McpSession>, Vec<McpTool>)> {
    let startup_timeout = config
        .startup_timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_STARTUP_TIMEOUT);
    let timeout_server = server_name.clone();
    timeout(startup_timeout, async move {
        let session = Arc::new(
            McpSession::start_sidecar(&docker, &workspace_volume, &image, server_name, config)
                .await?,
        );
        let tools = session.list_tools().await?;
        Ok((session, tools))
    })
    .await
    .map_err(|_| McpError::Timeout(timeout_server, "initialize".to_string()))?
}
