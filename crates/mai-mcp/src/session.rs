use mai_docker::DockerClient;
use mai_protocol::{McpServerConfig, McpServerTransport};
use rmcp::model::{ListResourceTemplatesResult, ListResourcesResult, ReadResourceResult};
use serde_json::Value;

use crate::error::Result;
use crate::http::RmcpSession;
use crate::stdio::StdioMcpSession;
use crate::types::McpTool;

pub(crate) enum McpSession {
    Stdio(Box<StdioMcpSession>),
    Http(Box<RmcpSession>),
}

impl McpSession {
    pub(crate) async fn start(
        docker: &DockerClient,
        container_id: &str,
        server_name: String,
        config: McpServerConfig,
    ) -> Result<Self> {
        match config.transport {
            McpServerTransport::Stdio => {
                let session =
                    StdioMcpSession::start(docker, container_id, server_name, config).await?;
                Ok(Self::Stdio(Box::new(session)))
            }
            McpServerTransport::StreamableHttp => {
                let session = RmcpSession::start_http(server_name, config).await?;
                Ok(Self::Http(Box::new(session)))
            }
        }
    }

    pub(crate) async fn start_sidecar(
        docker: &DockerClient,
        workspace_volume: &str,
        image: &str,
        server_name: String,
        config: McpServerConfig,
    ) -> Result<Self> {
        match config.transport {
            McpServerTransport::Stdio => {
                let session = StdioMcpSession::start_sidecar(
                    docker,
                    workspace_volume,
                    image,
                    server_name,
                    config,
                )
                .await?;
                Ok(Self::Stdio(Box::new(session)))
            }
            McpServerTransport::StreamableHttp => {
                let session = RmcpSession::start_http(server_name, config).await?;
                Ok(Self::Http(Box::new(session)))
            }
        }
    }

    pub(crate) async fn list_tools(&self) -> Result<Vec<McpTool>> {
        match self {
            Self::Stdio(session) => session.list_tools().await,
            Self::Http(session) => session.list_tools().await,
        }
    }

    pub(crate) async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        match self {
            Self::Stdio(session) => session.call_tool(name, arguments).await,
            Self::Http(session) => session.call_tool(name, arguments).await,
        }
    }

    pub(crate) async fn list_resources(
        &self,
        cursor: Option<String>,
    ) -> Result<ListResourcesResult> {
        match self {
            Self::Stdio(session) => session.list_resources(cursor).await,
            Self::Http(session) => session.list_resources(cursor).await,
        }
    }

    pub(crate) async fn list_resource_templates(
        &self,
        cursor: Option<String>,
    ) -> Result<ListResourceTemplatesResult> {
        match self {
            Self::Stdio(session) => session.list_resource_templates(cursor).await,
            Self::Http(session) => session.list_resource_templates(cursor).await,
        }
    }

    pub(crate) async fn read_resource(&self, uri: &str) -> Result<ReadResourceResult> {
        match self {
            Self::Stdio(session) => session.read_resource(uri).await,
            Self::Http(session) => session.read_resource(uri).await,
        }
    }

    pub(crate) async fn shutdown(&self) {
        match self {
            Self::Stdio(session) => session.shutdown().await,
            Self::Http(session) => session.shutdown().await,
        }
    }
}
