use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;

use axum::response::sse::Event;
use futures::{Stream, StreamExt};
use mai_protocol::ServiceEvent;
use tokio_stream::once;
use tokio_stream::wrappers::BroadcastStream;

const SSE_REPLAY_LIMIT: usize = 1_000;

pub(crate) type EventStream = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

pub(crate) struct EventStreamService {
    store: Arc<mai_store::ConfigStore>,
    runtime: Arc<mai_runtime::AgentRuntime>,
}

impl EventStreamService {
    pub(crate) fn new(
        store: Arc<mai_store::ConfigStore>,
        runtime: Arc<mai_runtime::AgentRuntime>,
    ) -> Self {
        Self { store, runtime }
    }

    pub(crate) async fn stream_after(
        &self,
        last_event_id: Option<u64>,
    ) -> Result<EventStream, mai_store::StoreError> {
        let initial = once(Ok(Event::default().comment("connected")));
        let replay = if let Some(last_event_id) = last_event_id {
            self.store
                .service_events_after(last_event_id, SSE_REPLAY_LIMIT)
                .await?
        } else {
            Vec::new()
        };
        let replay = tokio_stream::iter(replay.into_iter().map(|event| Ok(sse_event(event))));
        let events =
            BroadcastStream::new(self.runtime.subscribe()).filter_map(|event| async move {
                match event {
                    Ok(event) => Some(Ok(sse_event(event))),
                    Err(err) => {
                        tracing::warn!("SSE broadcast lagged or closed: {err}");
                        None
                    }
                }
            });
        Ok(Box::pin(initial.chain(replay).chain(events)))
    }
}

fn sse_event(event: ServiceEvent) -> Event {
    Event::default()
        .id(event.sequence.to_string())
        .event(event_name(&event))
        .json_data(event)
        .unwrap_or_else(|_| Event::default().data("{}"))
}

fn event_name(event: &ServiceEvent) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{
        AgentId, ServiceEvent, ServiceEventKind, SessionId, SkillActivationInfo, SkillScope, TurnId,
    };

    #[test]
    fn skills_activated_event_has_sse_name() {
        let event = ServiceEvent {
            sequence: 1,
            timestamp: mai_protocol::now(),
            kind: ServiceEventKind::SkillsActivated {
                agent_id: AgentId::new_v4(),
                session_id: Some(SessionId::new_v4()),
                turn_id: TurnId::new_v4(),
                skills: vec![SkillActivationInfo {
                    name: "demo".to_string(),
                    display_name: Some("Demo".to_string()),
                    path: std::path::PathBuf::from("/tmp/demo/SKILL.md"),
                    scope: SkillScope::Project,
                }],
            },
        };

        assert_eq!(event_name(&event), "skills_activated");
    }

    #[test]
    fn plan_updated_event_has_sse_name() {
        let event = ServiceEvent {
            sequence: 1,
            timestamp: mai_protocol::now(),
            kind: ServiceEventKind::PlanUpdated {
                task_id: TurnId::new_v4(),
                plan: mai_protocol::TaskPlan::default(),
            },
        };

        assert_eq!(event_name(&event), "plan_updated");
    }
}
