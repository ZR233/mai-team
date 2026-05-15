use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use mai_protocol::{
    AgentId, CreateProjectRequest, CreateProjectResponse, ProjectId, ProjectReviewRunDetail,
    ProjectReviewRunsResponse, SendMessageRequest, SendMessageResponse, SessionId,
    SkillsListResponse, UpdateProjectRequest, UpdateProjectResponse,
};

use super::state::{ApiError, AppState};

const DEFAULT_REVIEW_RUNS_PAGE_SIZE: usize = 50;

#[derive(Debug, Deserialize)]
pub(crate) struct ProjectDetailQuery {
    agent_id: Option<AgentId>,
    session_id: Option<SessionId>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProjectReviewRunsQuery {
    offset: Option<usize>,
    limit: Option<usize>,
}

pub(crate) async fn list_projects(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<Vec<mai_protocol::ProjectSummary>>, ApiError> {
    Ok(Json(state.runtime.list_projects().await))
}

pub(crate) async fn create_project(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateProjectRequest>,
) -> std::result::Result<Json<CreateProjectResponse>, ApiError> {
    let project = state.runtime.create_project(request).await?;
    Ok(Json(CreateProjectResponse { project }))
}

pub(crate) async fn get_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
    Query(query): Query<ProjectDetailQuery>,
) -> std::result::Result<Json<mai_protocol::ProjectDetail>, ApiError> {
    Ok(Json(
        state
            .runtime
            .get_project(id, query.agent_id, query.session_id)
            .await?,
    ))
}

pub(crate) async fn update_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
    Json(request): Json<UpdateProjectRequest>,
) -> std::result::Result<Json<UpdateProjectResponse>, ApiError> {
    let project = state.runtime.update_project(id, request).await?;
    Ok(Json(UpdateProjectResponse { project }))
}

pub(crate) async fn send_project_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
    Json(request): Json<SendMessageRequest>,
) -> std::result::Result<Json<SendMessageResponse>, ApiError> {
    let turn_id = state.runtime.send_project_message(id, request).await?;
    Ok(Json(SendMessageResponse { turn_id }))
}

pub(crate) async fn list_project_review_runs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
    Query(query): Query<ProjectReviewRunsQuery>,
) -> std::result::Result<Json<ProjectReviewRunsResponse>, ApiError> {
    Ok(Json(
        state
            .runtime
            .list_project_review_runs(id, query.offset.unwrap_or(0), query.limit.unwrap_or(DEFAULT_REVIEW_RUNS_PAGE_SIZE))
            .await?,
    ))
}

pub(crate) async fn get_project_review_run(
    State(state): State<Arc<AppState>>,
    Path((id, run_id)): Path<(ProjectId, String)>,
) -> std::result::Result<Json<ProjectReviewRunDetail>, ApiError> {
    let run_id = run_id.parse().map_err(|err| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: format!("invalid review run id: {err}"),
    })?;
    Ok(Json(
        state.runtime.get_project_review_run(id, run_id).await?,
    ))
}

pub(crate) async fn list_project_skills(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
) -> std::result::Result<Json<SkillsListResponse>, ApiError> {
    Ok(Json(state.runtime.list_project_skills(id).await?))
}

pub(crate) async fn detect_project_skills(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
) -> std::result::Result<Json<SkillsListResponse>, ApiError> {
    Ok(Json(state.runtime.detect_project_skills(id).await?))
}

pub(crate) async fn cancel_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.cancel_project(id).await?;
    Ok(StatusCode::ACCEPTED)
}

pub(crate) async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.delete_project(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
