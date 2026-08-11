use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use serde::Deserialize;

use mai_protocol::{
    AgentId, SendMessageRequest, SendMessageResponse, ThreadSnapshot, ThreadTurnPage,
};

use super::state::{ApiError, AppState};
use crate::services::events::ThreadEventStreamService;

const DEFAULT_TURN_PAGE_SIZE: usize = 50;
const MAX_TURN_PAGE_SIZE: usize = 200;

#[derive(Debug, Deserialize)]
pub(crate) struct ThreadTurnsQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

pub(crate) async fn get_thread(
    State(state): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
) -> Result<Json<ThreadSnapshot>, ApiError> {
    Ok(Json(state.runtime.thread_snapshot(thread_id).await?))
}

pub(crate) async fn list_thread_turns(
    State(state): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
    Query(query): Query<ThreadTurnsQuery>,
) -> Result<Json<ThreadTurnPage>, ApiError> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_TURN_PAGE_SIZE)
        .clamp(1, MAX_TURN_PAGE_SIZE);
    Ok(Json(
        state
            .runtime
            .thread_turns(thread_id, query.cursor.as_deref(), limit)
            .await?,
    ))
}

pub(crate) async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
    Json(request): Json<SendMessageRequest>,
) -> Result<Json<SendMessageResponse>, ApiError> {
    let agent_id = AgentId::parse_str(&thread_id).map_err(|error| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: format!("invalid product Thread id `{thread_id}`: {error}"),
    })?;
    let turn_id = state
        .runtime
        .send_message(agent_id, request.message, request.skill_mentions)
        .await?;
    Ok(Json(SendMessageResponse { turn_id }))
}

pub(crate) async fn events(
    State(state): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, ApiError> {
    tracing::debug!(%thread_id, "Thread SSE connection opened");
    let stream = ThreadEventStreamService::new(Arc::clone(&state.runtime))
        .stream(thread_id)
        .await?;
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
