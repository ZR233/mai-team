use thiserror::Error;

#[derive(Debug, Error)]
pub enum DockerError {
    #[error("docker is not available: {0}")]
    NotAvailable(String),
    #[error("docker command failed: {0}")]
    CommandFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("utf8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid docker image: {0}")]
    InvalidImage(String),
    #[error("docker command cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, DockerError>;
