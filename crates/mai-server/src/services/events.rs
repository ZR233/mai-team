use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;

use axum::response::sse::Event;
use futures::{Stream, StreamExt};
use mai_protocol::{MaiProductEventEnvelope, SessionEventPosition, SessionStreamFrame};
use tokio_stream::once;
use tokio_stream::wrappers::BroadcastStream;

const SSE_REPLAY_LIMIT: usize = 1_000;

pub(crate) type EventStream = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

pub(crate) struct SessionEventStreamService {
    runtime: Arc<mai_runtime::AgentRuntime>,
}

impl SessionEventStreamService {
    pub(crate) fn new(runtime: Arc<mai_runtime::AgentRuntime>) -> Self {
        Self { runtime }
    }

    pub(crate) async fn stream(
        &self,
        session_id: mai_protocol::SessionId,
        after_sequence: Option<u64>,
    ) -> Result<EventStream, mai_runtime::RuntimeError> {
        let subscription = self
            .runtime
            .subscribe_session_events(session_id, after_sequence)
            .await?;
        let frames = futures::stream::unfold(subscription, |mut subscription| async move {
            subscription.recv().await.map(|frame| {
                let event = session_sse_frame(frame);
                (Ok(event), subscription)
            })
        });
        Ok(Box::pin(frames))
    }
}

pub(crate) struct EventStreamService {
    store: Arc<mai_store::MaiStore>,
    runtime: Arc<mai_runtime::AgentRuntime>,
}

fn session_sse_frame(frame: SessionStreamFrame) -> Event {
    let event_name = match &frame {
        SessionStreamFrame::Snapshot { .. } => "snapshot",
        SessionStreamFrame::Event { .. } => "event",
        SessionStreamFrame::ResyncRequired { .. } => "resyncRequired",
    };
    let durable_sequence = match &frame {
        SessionStreamFrame::Event { event } => match event.position {
            SessionEventPosition::Durable { sequence } => Some(sequence),
            SessionEventPosition::Transient { revision: _ } => None,
        },
        SessionStreamFrame::Snapshot { .. } | SessionStreamFrame::ResyncRequired { .. } => None,
    };
    let event = Event::default().event(event_name);
    let event = match durable_sequence {
        Some(sequence) => event.id(sequence.to_string()),
        None => event,
    };
    event.json_data(frame).unwrap_or_else(|error| {
        tracing::error!(error = %error, "failed to serialize session SSE frame");
        Event::default().event("resyncRequired").data(
            r#"{"type":"resyncRequired","reason":{"type":"projectionInvariant","message":"serialization failed"}}"#,
        )
    })
}

impl EventStreamService {
    pub(crate) fn new(
        store: Arc<mai_store::MaiStore>,
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
                .product_events_after(last_event_id, SSE_REPLAY_LIMIT)
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

fn sse_event(event: MaiProductEventEnvelope) -> Event {
    let sequence = event.sequence;
    Event::default()
        .id(sequence.to_string())
        .event(event_name(&event))
        .json_data(event)
        .unwrap_or_else(|err| {
            tracing::error!(
                sequence,
                error = %err,
                "failed to serialize SSE event"
            );
            Event::default().data("{}")
        })
}

fn event_name(event: &MaiProductEventEnvelope) -> &'static str {
    match &event.kind {
        mai_protocol::MaiProductEventKind::AgentCreated { .. } => "agent_created",
        mai_protocol::MaiProductEventKind::AgentUpdated { .. } => "agent_updated",
        mai_protocol::MaiProductEventKind::AgentDeleted { .. } => "agent_deleted",
        mai_protocol::MaiProductEventKind::TaskCreated { .. } => "task_created",
        mai_protocol::MaiProductEventKind::TaskUpdated { .. } => "task_updated",
        mai_protocol::MaiProductEventKind::TaskDeleted { .. } => "task_deleted",
        mai_protocol::MaiProductEventKind::ProjectCreated { .. } => "project_created",
        mai_protocol::MaiProductEventKind::ProjectUpdated { .. } => "project_updated",
        mai_protocol::MaiProductEventKind::ProjectDeleted { .. } => "project_deleted",
        mai_protocol::MaiProductEventKind::GithubWebhookReceived { .. } => {
            "github_webhook_received"
        }
        mai_protocol::MaiProductEventKind::ProjectReviewQueued { .. } => "project_review_queued",
        mai_protocol::MaiProductEventKind::McpServerStatusChanged { .. } => {
            "mcp_server_status_changed"
        }
        mai_protocol::MaiProductEventKind::OperationFailed { .. } => "operation_failed",
        mai_protocol::MaiProductEventKind::PlanUpdated { .. } => "plan_updated",
        mai_protocol::MaiProductEventKind::ArtifactCreated { .. } => "artifact_created",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{AgentId, MaiProductEventEnvelope, MaiProductEventKind, ProjectId, TaskId};
    use pretty_assertions::assert_eq;

    fn make_event(kind: MaiProductEventKind) -> MaiProductEventEnvelope {
        MaiProductEventEnvelope {
            sequence: 1,
            timestamp: mai_protocol::now(),
            kind,
        }
    }

    #[test]
    fn agent_deleted_event_name() {
        let event = make_event(MaiProductEventKind::AgentDeleted {
            agent_id: AgentId::new_v4(),
        });
        assert_eq!(event_name(&event), "agent_deleted");
    }

    #[test]
    fn task_deleted_event_name() {
        let event = make_event(MaiProductEventKind::TaskDeleted {
            task_id: TaskId::new_v4(),
        });
        assert_eq!(event_name(&event), "task_deleted");
    }

    #[test]
    fn project_deleted_event_name() {
        let event = make_event(MaiProductEventKind::ProjectDeleted {
            project_id: ProjectId::new_v4(),
        });
        assert_eq!(event_name(&event), "project_deleted");
    }

    #[test]
    fn github_webhook_received_event_name() {
        let event = make_event(MaiProductEventKind::GithubWebhookReceived {
            delivery_id: "d1".into(),
            event: "push".into(),
            action: None,
            repository_full_name: None,
            installation_id: None,
        });
        assert_eq!(event_name(&event), "github_webhook_received");
    }

    #[test]
    fn operation_failed_event_name() {
        let event = make_event(MaiProductEventKind::OperationFailed {
            scope: "project".into(),
            agent_id: None,
            message: "oops".into(),
        });
        assert_eq!(event_name(&event), "operation_failed");
    }

    #[test]
    fn mcp_server_status_changed_event_name() {
        let event = make_event(MaiProductEventKind::McpServerStatusChanged {
            agent_id: AgentId::new_v4(),
            server: "test".into(),
            status: mai_protocol::McpStartupStatus::Ready,
            error: None,
        });
        assert_eq!(event_name(&event), "mcp_server_status_changed");
    }

    #[test]
    fn plan_updated_event_has_sse_name() {
        let event = make_event(MaiProductEventKind::PlanUpdated {
            task_id: TaskId::new_v4(),
            plan: mai_protocol::TaskPlan::default(),
        });

        assert_eq!(event_name(&event), "plan_updated");
    }
}
