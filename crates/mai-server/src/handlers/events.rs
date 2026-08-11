use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use serde::Deserialize;

use super::state::{ApiError, AppState};
use crate::services::events::EventStreamService;

#[derive(Debug, Deserialize)]
pub(crate) struct EventsQuery {
    last_event_id: Option<u64>,
}

pub(crate) async fn events(
    State(state): State<Arc<AppState>>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, ApiError> {
    tracing::debug!(last_event_id = ?query.last_event_id, "SSE connection opened");
    let service = EventStreamService::new(Arc::clone(&state.store), Arc::clone(&state.runtime));
    let stream = service
        .stream_after(last_event_id_from_request(query, &headers))
        .await?;
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn last_event_id_from_request(query: EventsQuery, headers: &HeaderMap) -> Option<u64> {
    query.last_event_id.or_else(|| {
        headers
            .get("last-event-id")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
    })
}
