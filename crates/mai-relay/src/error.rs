use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RelayErrorKind {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("jwt error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

impl IntoResponse for RelayErrorKind {
    fn into_response(self) -> Response {
        let status = match self {
            RelayErrorKind::InvalidInput(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

pub(crate) type RelayResult<T> = std::result::Result<T, RelayErrorKind>;

pub(crate) fn error_code(error: &RelayErrorKind) -> &'static str {
    match error {
        RelayErrorKind::InvalidInput(_) => "invalid_input",
        RelayErrorKind::Db(_) => "database",
        RelayErrorKind::Json(_) => "json",
        RelayErrorKind::Http(_) => "http",
        RelayErrorKind::Jwt(_) => "jwt",
    }
}
