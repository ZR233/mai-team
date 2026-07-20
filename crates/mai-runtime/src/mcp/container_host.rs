use mai_docker::DockerClient;
use pl_core::{
    LocalMcpRuntimeHost, LocalMcpSession, McpConnectRequest, McpRuntimeHost, McpServerTransport,
};
use pl_protocol::PureError;

/// 在 Mai agent 容器内创建 stdio 字节流，并在 server 进程中建立 HTTP 连接。
///
/// Host 只拥有执行环境：MCP 握手、JSON-RPC、工具与资源协议全部复用 PL 的
/// `LocalMcpSession`，generation、探测、命名和健康状态也仍由 PL runtime 管理。
#[derive(Clone)]
pub(crate) struct ContainerMcpRuntimeHost {
    docker: DockerClient,
    container_id: String,
}

impl ContainerMcpRuntimeHost {
    pub(crate) fn new(docker: DockerClient, container_id: String) -> Self {
        Self {
            docker,
            container_id,
        }
    }
}

impl McpRuntimeHost for ContainerMcpRuntimeHost {
    type Error = PureError;
    type Session = LocalMcpSession;

    async fn connect(&self, request: McpConnectRequest) -> Result<Self::Session, Self::Error> {
        match request.server.config.transport {
            McpServerTransport::StreamableHttp => LocalMcpRuntimeHost.connect(request).await,
            McpServerTransport::Stdio => {
                let command = request.server.config.command.as_deref().ok_or_else(|| {
                    PureError::ConfigError(format!(
                        "mcp server '{}' stdio command is required",
                        request.server_id
                    ))
                })?;
                let env = request
                    .server
                    .config
                    .env
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<Vec<_>>();
                let child = self
                    .docker
                    .spawn_exec(
                        &self.container_id,
                        command,
                        &request.server.config.args,
                        request.server.config.cwd.as_deref(),
                        &env,
                    )
                    .map_err(|error| {
                        PureError::ConfigError(format!(
                            "mcp server '{}' failed to start container command: {error}",
                            request.server_id
                        ))
                    })?;
                LocalMcpSession::from_stdio_child(request.server_id, child).await
            }
        }
    }
}
