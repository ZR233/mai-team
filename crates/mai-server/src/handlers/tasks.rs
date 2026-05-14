use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;

use super::state::{ApiError, AppState};
use crate::services::artifacts::ArtifactService;
use mai_protocol::{
    AgentId, ApproveTaskPlanResponse, CreateTaskRequest, CreateTaskResponse,
    RequestPlanRevisionRequest, RequestPlanRevisionResponse, SendMessageRequest,
    SendMessageResponse, TaskId,
};

#[derive(Debug, Deserialize)]
pub(crate) struct TaskDetailQuery {
    agent_id: Option<AgentId>,
}

pub(crate) async fn list_tasks(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<Vec<mai_protocol::TaskSummary>>, ApiError> {
    Ok(Json(state.runtime.list_tasks().await))
}

pub(crate) async fn ensure_default_task(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<Option<mai_protocol::TaskSummary>>, ApiError> {
    Ok(Json(state.runtime.ensure_default_task().await?))
}

pub(crate) async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateTaskRequest>,
) -> std::result::Result<Json<CreateTaskResponse>, ApiError> {
    let task = state
        .runtime
        .create_task(request.title, request.message, request.docker_image)
        .await?;
    Ok(Json(CreateTaskResponse { task }))
}

pub(crate) async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<TaskId>,
    Query(query): Query<TaskDetailQuery>,
) -> std::result::Result<Json<mai_protocol::TaskDetail>, ApiError> {
    Ok(Json(state.runtime.get_task(id, query.agent_id).await?))
}

pub(crate) async fn send_task_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<TaskId>,
    Json(request): Json<SendMessageRequest>,
) -> std::result::Result<Json<SendMessageResponse>, ApiError> {
    let turn_id = state
        .runtime
        .send_task_message(id, request.message, request.skill_mentions)
        .await?;
    Ok(Json(SendMessageResponse { turn_id }))
}

pub(crate) async fn approve_task_plan(
    State(state): State<Arc<AppState>>,
    Path(id): Path<TaskId>,
) -> std::result::Result<Json<ApproveTaskPlanResponse>, ApiError> {
    let task = state.runtime.approve_task_plan(id).await?;
    Ok(Json(ApproveTaskPlanResponse { task }))
}

pub(crate) async fn request_plan_revision(
    State(state): State<Arc<AppState>>,
    Path(id): Path<TaskId>,
    Json(request): Json<RequestPlanRevisionRequest>,
) -> std::result::Result<Json<RequestPlanRevisionResponse>, ApiError> {
    let task = state
        .runtime
        .request_plan_revision(id, request.feedback)
        .await?;
    Ok(Json(RequestPlanRevisionResponse { task }))
}

pub(crate) async fn cancel_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<TaskId>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.cancel_task(id).await?;
    Ok(StatusCode::ACCEPTED)
}

pub(crate) async fn delete_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<TaskId>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.delete_task(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn list_artifacts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<TaskId>,
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
    let file = service
        .download_artifact(&id)
        .await
        .map_err(|e| match e.to_string() {
            msg if msg.contains("not found") => ApiError {
                status: StatusCode::NOT_FOUND,
                message: msg,
            },
            msg => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: msg,
            },
        })?;
    Ok(file.into_response())
}
