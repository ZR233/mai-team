use std::sync::Arc;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use clap::Parser;
use mai_protocol::ErrorResponse;

use mai_relay_client::RelayClient;

#[derive(Debug, Parser)]
#[command(author, version, about)]
pub(crate) struct Cli {
    #[arg(long = "data-path", value_name = "PATH")]
    pub(crate) data_path: Option<std::path::PathBuf>,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) runtime: Arc<mai_runtime::AgentRuntime>,
    pub(crate) store: Arc<mai_store::ConfigStore>,
    pub(crate) relay: Option<Arc<RelayClient>>,
}

#[derive(Debug)]
pub(crate) struct ApiError {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
}

impl From<mai_runtime::RuntimeError> for ApiError {
    fn from(value: mai_runtime::RuntimeError) -> Self {
        use mai_runtime::RuntimeError::*;
        let status = match &value {
            AgentNotFound(_)
            | TaskNotFound(_)
            | ProjectNotFound(_)
            | ProjectReviewRunNotFound(_) => StatusCode::NOT_FOUND,
            TurnNotFound { .. } => StatusCode::NOT_FOUND,
            SessionNotFound { .. } => StatusCode::NOT_FOUND,
            ToolTraceNotFound { .. } => StatusCode::NOT_FOUND,
            AgentBusy(_) | TaskBusy(_) => StatusCode::CONFLICT,
            InvalidInput(_) => StatusCode::BAD_REQUEST,
            MissingContainer(_) => StatusCode::CONFLICT,
            TurnCancelled => StatusCode::CONFLICT,
            Docker(_) | Model(_) | Mcp(_) | Store(_) | Skill(_) | Io(_) | Http(_) | Jwt(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        Self {
            status,
            message: value.to_string(),
        }
    }
}

impl From<mai_store::StoreError> for ApiError {
    fn from(value: mai_store::StoreError) -> Self {
        use mai_store::StoreError::*;
        let status = match &value {
            InvalidConfig(_) | Parse(_) => StatusCode::BAD_REQUEST,
            Toasty(_) | Io(_) | Json(_) | Sqlite(_) | Toml(_) | TomlSer(_) | Time(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        Self {
            status,
            message: value.to_string(),
        }
    }
}

impl ApiError {
    pub(crate) fn bad_request(message: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::error!(status = %self.status, error = %self.message, "request failed");
        }
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}
