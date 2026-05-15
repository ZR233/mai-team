use std::sync::Arc;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mai_protocol::ErrorResponse;

use crate::services::relay_manager::RelayManager;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) runtime: Arc<mai_runtime::AgentRuntime>,
    pub(crate) store: Arc<mai_store::ConfigStore>,
    pub(crate) relay: Arc<RelayManager>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use mai_runtime::RuntimeError;
    use pretty_assertions::assert_eq;

    fn agent_id() -> mai_protocol::AgentId {
        mai_protocol::AgentId::new_v4()
    }

    fn task_id() -> mai_protocol::TaskId {
        mai_protocol::TaskId::new_v4()
    }

    #[test]
    fn runtime_error_not_found_maps_to_404() {
        let err = RuntimeError::AgentNotFound(agent_id());
        let api: ApiError = err.into();
        assert_eq!(api.status, StatusCode::NOT_FOUND);

        let err = RuntimeError::TaskNotFound(task_id());
        let api: ApiError = err.into();
        assert_eq!(api.status, StatusCode::NOT_FOUND);

        let err = RuntimeError::TurnNotFound {
            agent_id: agent_id(),
            turn_id: mai_protocol::TurnId::new_v4(),
        };
        let api: ApiError = err.into();
        assert_eq!(api.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn runtime_error_busy_maps_to_409() {
        let err = RuntimeError::AgentBusy(agent_id());
        let api: ApiError = err.into();
        assert_eq!(api.status, StatusCode::CONFLICT);

        let err = RuntimeError::TaskBusy(task_id());
        let api: ApiError = err.into();
        assert_eq!(api.status, StatusCode::CONFLICT);
    }

    #[test]
    fn runtime_error_invalid_input_maps_to_400() {
        let err = RuntimeError::InvalidInput("bad request".into());
        let api: ApiError = err.into();
        assert_eq!(api.status, StatusCode::BAD_REQUEST);
        assert!(api.message.contains("bad request"));
    }

    #[test]
    fn runtime_error_turn_cancelled_maps_to_409() {
        let err = RuntimeError::TurnCancelled;
        let api: ApiError = err.into();
        assert_eq!(api.status, StatusCode::CONFLICT);
    }
}
