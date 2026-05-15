use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;

use super::state::{ApiError, AppState};
use crate::services::artifacts::ArtifactService;
use mai_protocol::{
    CreateEnvironmentRequest, CreateEnvironmentResponse, CreateSessionResponse, EnvironmentId,
    SendMessageRequest, SendMessageResponse, SessionId,
};

#[derive(Debug, Deserialize)]
pub(crate) struct EnvironmentDetailQuery {
    session_id: Option<SessionId>,
}

pub(crate) async fn list_environments(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<Vec<mai_protocol::EnvironmentSummary>>, ApiError> {
    Ok(Json(state.runtime.list_environments().await))
}

pub(crate) async fn ensure_default_environment(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<Option<mai_protocol::EnvironmentSummary>>, ApiError> {
    Ok(Json(state.runtime.ensure_default_environment().await?))
}

pub(crate) async fn create_environment(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateEnvironmentRequest>,
) -> std::result::Result<Json<CreateEnvironmentResponse>, ApiError> {
    let environment = state
        .runtime
        .create_environment(request.name, request.docker_image)
        .await?;
    Ok(Json(CreateEnvironmentResponse { environment }))
}

pub(crate) async fn get_environment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<EnvironmentId>,
    Query(query): Query<EnvironmentDetailQuery>,
) -> std::result::Result<Json<mai_protocol::EnvironmentDetail>, ApiError> {
    Ok(Json(
        state.runtime.get_environment(id, query.session_id).await?,
    ))
}

pub(crate) async fn create_conversation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<EnvironmentId>,
) -> std::result::Result<Json<CreateSessionResponse>, ApiError> {
    let session = state.runtime.create_environment_conversation(id).await?;
    Ok(Json(CreateSessionResponse { session }))
}

pub(crate) async fn send_conversation_message(
    State(state): State<Arc<AppState>>,
    Path((id, session_id)): Path<(EnvironmentId, SessionId)>,
    Json(request): Json<SendMessageRequest>,
) -> std::result::Result<Json<SendMessageResponse>, ApiError> {
    let turn_id = state
        .runtime
        .send_environment_message(id, session_id, request.message, request.skill_mentions)
        .await?;
    Ok(Json(SendMessageResponse { turn_id }))
}

pub(crate) async fn list_artifacts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<EnvironmentId>,
) -> std::result::Result<Json<Vec<mai_protocol::ArtifactInfo>>, ApiError> {
    let service = ArtifactService::new(Arc::clone(&state.store), Arc::clone(&state.runtime));
    let artifacts = service.list_artifacts(&id).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: e.to_string(),
    })?;
    Ok(Json(artifacts))
}

pub(crate) async fn download_artifact(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    let service = ArtifactService::new(Arc::clone(&state.store), Arc::clone(&state.runtime));
    let file = service.download_artifact(&id).await.map_err(|e| {
        let message = e.to_string();
        if message.contains("not found") {
            ApiError {
                status: StatusCode::NOT_FOUND,
                message,
            }
        } else {
            ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message,
            }
        }
    })?;
    Ok(file.into_response())
}
