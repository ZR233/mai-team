use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::StreamExt;
use serde::Deserialize;
use tokio_stream::once;
use tokio_stream::wrappers::BroadcastStream;

use mai_protocol::ServiceEvent;

use super::state::{ApiError, AppState};

const SSE_REPLAY_LIMIT: usize = 1_000;

#[derive(Debug, Deserialize)]
pub(crate) struct EventsQuery {
    last_event_id: Option<u64>,
}

pub(crate) async fn events(
    State(state): State<Arc<AppState>>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> std::result::Result<
    Sse<impl futures::Stream<Item = std::result::Result<Event, Infallible>>>,
    ApiError,
> {
    let initial = once(Ok(Event::default().comment("connected")));
    let last_event_id = query.last_event_id.or_else(|| {
        headers
            .get("last-event-id")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
    });
    let replay = if let Some(last_event_id) = last_event_id {
        state
            .store
            .service_events_after(last_event_id, SSE_REPLAY_LIMIT)
            .await?
    } else {
        Vec::new()
    };
    let replay = tokio_stream::iter(replay.into_iter().map(|event| Ok(sse_event(event))));
    let events = BroadcastStream::new(state.runtime.subscribe()).filter_map(|event| async move {
        match event {
            Ok(event) => Some(Ok(sse_event(event))),
            Err(err) => {
                tracing::warn!("SSE broadcast lagged or closed: {err}");
                None
            }
        }
    });
    let stream = initial.chain(replay).chain(events);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn sse_event(event: ServiceEvent) -> Event {
    Event::default()
        .id(event.sequence.to_string())
        .event(event_name(&event))
        .json_data(event)
        .unwrap_or_else(|_| Event::default().data("{}"))
}

pub(crate) fn event_name(event: &ServiceEvent) -> &'static str {
    match &event.kind {
        mai_protocol::ServiceEventKind::AgentCreated { .. } => "agent_created",
        mai_protocol::ServiceEventKind::AgentStatusChanged { .. } => "agent_status_changed",
        mai_protocol::ServiceEventKind::AgentUpdated { .. } => "agent_updated",
        mai_protocol::ServiceEventKind::AgentDeleted { .. } => "agent_deleted",
        mai_protocol::ServiceEventKind::TaskCreated { .. } => "task_created",
        mai_protocol::ServiceEventKind::TaskUpdated { .. } => "task_updated",
        mai_protocol::ServiceEventKind::TaskDeleted { .. } => "task_deleted",
        mai_protocol::ServiceEventKind::ProjectCreated { .. } => "project_created",
        mai_protocol::ServiceEventKind::ProjectUpdated { .. } => "project_updated",
        mai_protocol::ServiceEventKind::ProjectDeleted { .. } => "project_deleted",
        mai_protocol::ServiceEventKind::GithubWebhookReceived { .. } => "github_webhook_received",
        mai_protocol::ServiceEventKind::ProjectReviewQueued { .. } => "project_review_queued",
        mai_protocol::ServiceEventKind::TurnStarted { .. } => "turn_started",
        mai_protocol::ServiceEventKind::TurnCompleted { .. } => "turn_completed",
        mai_protocol::ServiceEventKind::ToolStarted { .. } => "tool_started",
        mai_protocol::ServiceEventKind::ToolCompleted { .. } => "tool_completed",
        mai_protocol::ServiceEventKind::ContextCompacted { .. } => "context_compacted",
        mai_protocol::ServiceEventKind::AgentMessage { .. } => "agent_message",
        mai_protocol::ServiceEventKind::AgentMessageDelta { .. } => "agent_message_delta",
        mai_protocol::ServiceEventKind::AgentMessageCompleted { .. } => "agent_message_completed",
        mai_protocol::ServiceEventKind::ReasoningDelta { .. } => "reasoning_delta",
        mai_protocol::ServiceEventKind::ReasoningCompleted { .. } => "reasoning_completed",
        mai_protocol::ServiceEventKind::ToolCallDelta { .. } => "tool_call_delta",
        mai_protocol::ServiceEventKind::SkillsActivated { .. } => "skills_activated",
        mai_protocol::ServiceEventKind::McpServerStatusChanged { .. } => {
            "mcp_server_status_changed"
        }
        mai_protocol::ServiceEventKind::Error { .. } => "error",
        mai_protocol::ServiceEventKind::TodoListUpdated { .. } => "todo_list_updated",
        mai_protocol::ServiceEventKind::PlanUpdated { .. } => "plan_updated",
        mai_protocol::ServiceEventKind::UserInputRequested { .. } => "user_input_requested",
        mai_protocol::ServiceEventKind::ArtifactCreated { .. } => "artifact_created",
    }
}
