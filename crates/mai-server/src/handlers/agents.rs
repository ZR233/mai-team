use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use mai_protocol::{
    AgentId, AgentLogsResponse, CreateAgentRequest, CreateAgentResponse, CreateSessionResponse,
    FileUploadRequest, FileUploadResponse, SendMessageRequest, SendMessageResponse, SessionId,
    ToolTraceDetail, ToolTraceListResponse, TurnId, UpdateAgentRequest, UpdateAgentResponse,
};
use mai_store::AgentLogFilter;
use mai_store::ToolTraceFilter;

use super::state::{ApiError, AppState};

fn bounded_api_limit(limit: Option<usize>, default: usize, max: usize) -> usize {
    limit.unwrap_or(default).clamp(1, max)
}

#[derive(Debug, Deserialize)]
pub(crate) struct AgentDetailQuery {
    session_id: Option<SessionId>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AgentLogsQuery {
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
    level: Option<String>,
    category: Option<String>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ToolTraceListQuery {
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DownloadQuery {
    path: String,
}

pub(crate) async fn list_agents(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<Vec<mai_protocol::AgentSummary>>, ApiError> {
    Ok(Json(state.runtime.list_agents().await))
}

pub(crate) async fn create_agent(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateAgentRequest>,
) -> std::result::Result<Json<CreateAgentResponse>, ApiError> {
    let agent = state.runtime.create_agent(request).await?;
    Ok(Json(CreateAgentResponse { agent }))
}

pub(crate) async fn get_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
    Query(query): Query<AgentDetailQuery>,
) -> std::result::Result<Json<mai_protocol::AgentDetail>, ApiError> {
    Ok(Json(state.runtime.get_agent(id, query.session_id).await?))
}

pub(crate) async fn update_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
    Json(request): Json<UpdateAgentRequest>,
) -> std::result::Result<Json<UpdateAgentResponse>, ApiError> {
    let agent = state.runtime.update_agent(id, request).await?;
    Ok(Json(UpdateAgentResponse { agent }))
}

pub(crate) async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
    Json(request): Json<SendMessageRequest>,
) -> std::result::Result<Json<SendMessageResponse>, ApiError> {
    let turn_id = state
        .runtime
        .send_message(id, None, request.message, request.skill_mentions)
        .await?;
    Ok(Json(SendMessageResponse { turn_id }))
}

pub(crate) async fn create_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
) -> std::result::Result<Json<CreateSessionResponse>, ApiError> {
    let session = state.runtime.create_session(id).await?;
    Ok(Json(CreateSessionResponse { session }))
}

pub(crate) async fn send_session_message(
    State(state): State<Arc<AppState>>,
    Path((id, session_id)): Path<(AgentId, SessionId)>,
    Json(request): Json<SendMessageRequest>,
) -> std::result::Result<Json<SendMessageResponse>, ApiError> {
    let turn_id = state
        .runtime
        .send_message(
            id,
            Some(session_id),
            request.message,
            request.skill_mentions,
        )
        .await?;
    Ok(Json(SendMessageResponse { turn_id }))
}

pub(crate) async fn list_agent_logs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
    Query(query): Query<AgentLogsQuery>,
) -> std::result::Result<Json<AgentLogsResponse>, ApiError> {
    let limit = bounded_api_limit(query.limit, 100, 500);
    Ok(Json(
        state
            .runtime
            .agent_logs(
                id,
                AgentLogFilter {
                    session_id: query.session_id,
                    turn_id: query.turn_id,
                    level: query.level.filter(|value| !value.trim().is_empty()),
                    category: query.category.filter(|value| !value.trim().is_empty()),
                    since: query.since,
                    until: query.until,
                    offset: query.offset.unwrap_or(0),
                    limit,
                },
            )
            .await?,
    ))
}

pub(crate) async fn list_tool_traces(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
    Query(query): Query<ToolTraceListQuery>,
) -> std::result::Result<Json<ToolTraceListResponse>, ApiError> {
    let limit = bounded_api_limit(query.limit, 100, 500);
    Ok(Json(
        state
            .runtime
            .tool_traces(
                id,
                ToolTraceFilter {
                    session_id: query.session_id,
                    turn_id: query.turn_id,
                    offset: query.offset.unwrap_or(0),
                    limit,
                },
            )
            .await?,
    ))
}

pub(crate) async fn get_tool_trace(
    State(state): State<Arc<AppState>>,
    Path((id, call_id)): Path<(AgentId, String)>,
) -> std::result::Result<Json<ToolTraceDetail>, ApiError> {
    Ok(Json(state.runtime.tool_trace(id, None, call_id).await?))
}

pub(crate) async fn get_session_tool_trace(
    State(state): State<Arc<AppState>>,
    Path((id, session_id, call_id)): Path<(AgentId, SessionId, String)>,
) -> std::result::Result<Json<ToolTraceDetail>, ApiError> {
    Ok(Json(
        state
            .runtime
            .tool_trace(id, Some(session_id), call_id)
            .await?,
    ))
}

pub(crate) async fn upload_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
    Json(request): Json<FileUploadRequest>,
) -> std::result::Result<Json<FileUploadResponse>, ApiError> {
    let bytes = state
        .runtime
        .upload_file(id, request.path.clone(), request.content_base64)
        .await?;
    Ok(Json(FileUploadResponse {
        path: request.path,
        bytes,
    }))
}

pub(crate) async fn download_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
    Query(query): Query<DownloadQuery>,
) -> std::result::Result<Response, ApiError> {
    let bytes = state.runtime.download_file_tar(id, query.path).await?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-tar")
        .body(Body::from(bytes))
        .expect("response builder"))
}

pub(crate) async fn cancel_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.cancel_agent(id).await?;
    Ok(StatusCode::ACCEPTED)
}

pub(crate) async fn cancel_agent_turn(
    State(state): State<Arc<AppState>>,
    Path((id, turn_id)): Path<(AgentId, TurnId)>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.cancel_agent_turn(id, turn_id).await?;
    Ok(StatusCode::ACCEPTED)
}

pub(crate) async fn cancel_agent_colon(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> std::result::Result<StatusCode, ApiError> {
    let id = id.strip_suffix(":cancel").unwrap_or(&id);
    let id = id.parse::<AgentId>().map_err(|err| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: format!("invalid agent id: {err}"),
    })?;
    state.runtime.cancel_agent(id).await?;
    Ok(StatusCode::ACCEPTED)
}

pub(crate) async fn delete_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.delete_agent(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
