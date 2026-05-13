use reqwest::StatusCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("request to {endpoint} failed: {source}")]
    Request {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("request to {endpoint} returned {status}: {body}")]
    Api {
        endpoint: String,
        status: StatusCode,
        body: String,
    },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("stream error: {0}")]
    Stream(String),
    #[error("request cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, ModelError>;
