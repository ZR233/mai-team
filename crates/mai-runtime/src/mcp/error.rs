use mai_docker::DockerError;
use rmcp::service::ServiceError;
use thiserror::Error;

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
