use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;

use axum::response::sse::Event;
use futures::{Stream, StreamExt};
use mai_protocol::{MaiProductEventEnvelope, ThreadSubscriptionUpdate};
use tokio_stream::once;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;

const SSE_REPLAY_LIMIT: usize = 1_000;

pub(crate) type EventStream = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

/// 让长连接流服从 server 唯一的关闭信号，避免 HTTP graceful shutdown 永久等待 SSE。
pub(crate) fn stop_on_shutdown(stream: EventStream, shutdown: CancellationToken) -> EventStream {
    Box::pin(stream.take_until(shutdown.cancelled_owned()))
}

pub(crate) struct ThreadEventStreamService {
    runtime: Arc<mai_runtime::AgentRuntime>,
}

impl ThreadEventStreamService {
    pub(crate) fn new(runtime: Arc<mai_runtime::AgentRuntime>) -> Self {
        Self { runtime }
    }

    pub(crate) async fn stream(
        &self,
        thread_id: String,
    ) -> Result<EventStream, mai_runtime::RuntimeError> {
        let subscription = self.runtime.subscribe_thread(thread_id).await?;
        let updates = futures::stream::unfold(subscription, |mut subscription| async move {
            subscription.recv().await.map(|update| {
                let event = thread_sse_update(update);
                (Ok(event), subscription)
            })
        });
        Ok(Box::pin(updates))
    }
}

pub(crate) struct EventStreamService {
    store: Arc<mai_store::MaiStore>,
    runtime: Arc<mai_runtime::AgentRuntime>,
}

fn thread_sse_update(update: ThreadSubscriptionUpdate) -> Event {
    let event_name = match &update {
        ThreadSubscriptionUpdate::Snapshot { .. } => "snapshot",
        ThreadSubscriptionUpdate::Notification { .. } => "notification",
    };
    let revision = match &update {
        ThreadSubscriptionUpdate::Snapshot { .. } => None,
        ThreadSubscriptionUpdate::Notification { notification } => Some(notification.revision),
    };
    let event = Event::default().event(event_name);
    let event = match revision {
        Some(revision) => event.id(revision.to_string()),
        None => event,
    };
    event.json_data(update).unwrap_or_else(|error| {
        tracing::error!(error = %error, "failed to serialize Thread SSE update");
        Event::default().event("serializationError").data("{}")
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
    use std::time::Duration;

    #[tokio::test]
    async fn server_shutdown_ends_sse_without_a_new_event() {
        let shutdown = CancellationToken::new();
        let pending: EventStream = Box::pin(futures::stream::pending());
        let mut stream = stop_on_shutdown(pending, shutdown.clone());

        shutdown.cancel();

        let item = tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .expect("shutdown must wake the pending SSE stream");
        assert!(item.is_none());
    }

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
