use std::future::Future;
use std::sync::Arc;

use mai_protocol::{ProjectId, RelayAckStatus, RelayEvent, RelayEventKind, ServiceEventKind};
use mai_relay_client::RelayClient;
use mai_runtime::{
    AgentRuntime, ProjectReviewQueueRequest, ProjectReviewQueueSummary, RuntimeError,
};
use serde_json::Value;

pub(crate) async fn install_relay_event_handler(
    relay: Arc<RelayClient>,
    runtime: Arc<AgentRuntime>,
) {
    relay
        .set_event_handler(move |event| {
            let runtime = Arc::clone(&runtime);
            async move {
                process_event(&runtime, &event)
                    .await
                    .map_err(|err| err.to_string())
            }
        })
        .await;
}

/// 提供处理 relay 事件所需的运行时副作用。
///
/// 实现方需要发布收到的 webhook 事件、解析目标项目、只把 PR 相关信号放入
/// relay 队列，并通过显式方法处理 push 同步等非 PR 副作用。
trait RelayEventRuntime: Send + Sync {
    fn publish_external_event(&self, kind: ServiceEventKind) -> impl Future<Output = ()> + Send;

    fn find_project_for_github_event(
        &self,
        installation_id: Option<u64>,
        repository_id: Option<u64>,
        repository_full_name: Option<&str>,
    ) -> impl Future<Output = Option<ProjectId>> + Send;

    fn enqueue_project_review_relay_signal(
        &self,
        request: ProjectReviewQueueRequest,
    ) -> impl Future<Output = Result<ProjectReviewQueueSummary, RuntimeError>> + Send;

    fn handle_project_push_event(
        &self,
        project_id: ProjectId,
        payload: &Value,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send;
}

impl RelayEventRuntime for Arc<AgentRuntime> {
    fn publish_external_event(&self, kind: ServiceEventKind) -> impl Future<Output = ()> + Send {
        AgentRuntime::publish_external_event(self, kind)
    }

    fn find_project_for_github_event(
        &self,
        installation_id: Option<u64>,
        repository_id: Option<u64>,
        repository_full_name: Option<&str>,
    ) -> impl Future<Output = Option<ProjectId>> + Send {
        AgentRuntime::find_project_for_github_event(
            self,
            installation_id,
            repository_id,
            repository_full_name,
        )
    }

    fn enqueue_project_review_relay_signal(
        &self,
        request: ProjectReviewQueueRequest,
    ) -> impl Future<Output = Result<ProjectReviewQueueSummary, RuntimeError>> + Send {
        AgentRuntime::enqueue_project_review_relay_signal(self, request)
    }

    fn handle_project_push_event(
        &self,
        project_id: ProjectId,
        payload: &Value,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        AgentRuntime::handle_project_push_event(self, project_id, payload)
    }
}

async fn process_event(
    runtime: &impl RelayEventRuntime,
    event: &RelayEvent,
) -> Result<RelayAckStatus, RuntimeError> {
    tracing::debug!(
        delivery_id = %event.delivery_id,
        kind = %event.kind.as_github_event(),
        "processing relay event"
    );
    let event_name = event.kind.as_github_event().to_string();
    let action = event
        .payload
        .get("action")
        .and_then(Value::as_str)
        .map(str::to_string);
    let repository = event.payload.get("repository");
    let repository_full_name = repository
        .and_then(|repo| repo.get("full_name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let repository_id = repository
        .and_then(|repo| repo.get("id"))
        .and_then(Value::as_u64);
    let installation_id = event
        .payload
        .get("installation")
        .and_then(|installation| installation.get("id"))
        .and_then(Value::as_u64);

    runtime
        .publish_external_event(ServiceEventKind::GithubWebhookReceived {
            delivery_id: event.delivery_id.clone(),
            event: event_name.clone(),
            action: action.clone(),
            repository_full_name: repository_full_name.clone(),
            installation_id,
        })
        .await;

    let Some(project_id) = runtime
        .find_project_for_github_event(
            installation_id,
            repository_id,
            repository_full_name.as_deref(),
        )
        .await
    else {
        return Ok(RelayAckStatus::Ignored);
    };

    match event.kind {
        RelayEventKind::PullRequest => {
            if !matches!(
                action.as_deref(),
                Some("opened" | "reopened" | "synchronize" | "ready_for_review")
            ) {
                return Ok(RelayAckStatus::Ignored);
            }
            let Some(pr) = pull_request_number(&event.payload) else {
                return Ok(RelayAckStatus::Ignored);
            };
            let summary = runtime
                .enqueue_project_review_relay_signal(ProjectReviewQueueRequest {
                    project_id,
                    pr,
                    head_sha: mai_relay_client::head_sha(&event.payload),
                    delivery_id: Some(event.delivery_id.clone()),
                    reason: event_name,
                })
                .await?;
            Ok(ack_status_for_queue(summary))
        }
        RelayEventKind::CheckRun | RelayEventKind::CheckSuite => {
            if action.as_deref() != Some("completed") {
                return Ok(RelayAckStatus::Ignored);
            }
            let mut processed = false;
            let head_sha = mai_relay_client::head_sha(&event.payload);
            for pr in mai_relay_client::associated_pull_requests(&event.payload) {
                let summary = runtime
                    .enqueue_project_review_relay_signal(ProjectReviewQueueRequest {
                        project_id,
                        pr,
                        head_sha: head_sha.clone(),
                        delivery_id: Some(event.delivery_id.clone()),
                        reason: event_name.clone(),
                    })
                    .await?;
                processed = processed || ack_status_for_queue(summary) == RelayAckStatus::Processed;
            }
            Ok(if processed {
                RelayAckStatus::Processed
            } else {
                RelayAckStatus::Ignored
            })
        }
        RelayEventKind::Push => {
            runtime
                .handle_project_push_event(project_id, &event.payload)
                .await?;
            Ok(RelayAckStatus::Processed)
        }
        RelayEventKind::Installation
        | RelayEventKind::InstallationRepositories
        | RelayEventKind::Other(_) => Ok(RelayAckStatus::Ignored),
    }
}

fn pull_request_number(payload: &Value) -> Option<u64> {
    payload
        .get("pull_request")
        .and_then(|pr| pr.get("number"))
        .and_then(Value::as_u64)
        .or_else(|| payload.get("number").and_then(Value::as_u64))
}

fn ack_status_for_queue(summary: mai_runtime::ProjectReviewQueueSummary) -> RelayAckStatus {
    if summary.queued.is_empty() && summary.deduped.is_empty() {
        RelayAckStatus::Ignored
    } else {
        RelayAckStatus::Processed
    }
}

#[cfg(test)]
mod tests {
    use mai_protocol::RelayEvent;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use super::*;

    struct FakeRelayRuntime {
        project_id: ProjectId,
        relay_requests: Mutex<Vec<ProjectReviewQueueRequest>>,
        push_events: Mutex<Vec<ProjectId>>,
        external_events: Mutex<Vec<ServiceEventKind>>,
    }

    impl FakeRelayRuntime {
        fn new(project_id: ProjectId) -> Self {
            Self {
                project_id,
                relay_requests: Mutex::new(Vec::new()),
                push_events: Mutex::new(Vec::new()),
                external_events: Mutex::new(Vec::new()),
            }
        }
    }

    impl RelayEventRuntime for FakeRelayRuntime {
        async fn publish_external_event(&self, kind: ServiceEventKind) {
            self.external_events.lock().await.push(kind);
        }

        async fn find_project_for_github_event(
            &self,
            _installation_id: Option<u64>,
            _repository_id: Option<u64>,
            _repository_full_name: Option<&str>,
        ) -> Option<ProjectId> {
            Some(self.project_id)
        }

        async fn enqueue_project_review_relay_signal(
            &self,
            request: ProjectReviewQueueRequest,
        ) -> Result<ProjectReviewQueueSummary, RuntimeError> {
            let pr = request.pr;
            self.relay_requests.lock().await.push(request);
            Ok(ProjectReviewQueueSummary {
                queued: vec![pr],
                deduped: Vec::new(),
                ignored: Vec::new(),
            })
        }

        async fn handle_project_push_event(
            &self,
            project_id: ProjectId,
            _payload: &Value,
        ) -> Result<(), RuntimeError> {
            self.push_events.lock().await.push(project_id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn pull_request_event_queues_relay_signal() {
        let project_id = Uuid::new_v4();
        let runtime = FakeRelayRuntime::new(project_id);
        let event = RelayEvent {
            sequence: 1,
            delivery_id: "delivery-1".to_string(),
            kind: RelayEventKind::PullRequest,
            payload: json!({
                "action": "synchronize",
                "installation": { "id": 123 },
                "repository": {
                    "id": 42,
                    "full_name": "owner/repo"
                },
                "pull_request": {
                    "number": 9,
                    "head": { "sha": "head-9" }
                }
            }),
        };

        let ack = process_event(&runtime, &event)
            .await
            .expect("process event");

        assert_eq!(RelayAckStatus::Processed, ack);
        let requests = runtime.relay_requests.lock().await;
        assert_eq!(1, requests.len());
        let request = requests.first().expect("relay request");
        assert_eq!(project_id, request.project_id);
        assert_eq!(9, request.pr);
        assert_eq!(Some("head-9".to_string()), request.head_sha);
        assert_eq!(Some("delivery-1".to_string()), request.delivery_id);
        assert_eq!("pull_request", request.reason);
        assert_eq!(Vec::<ProjectId>::new(), *runtime.push_events.lock().await);
    }

    #[tokio::test]
    async fn push_event_still_uses_push_handler() {
        let project_id = Uuid::new_v4();
        let runtime = FakeRelayRuntime::new(project_id);
        let event = RelayEvent {
            sequence: 1,
            delivery_id: "delivery-2".to_string(),
            kind: RelayEventKind::Push,
            payload: json!({
                "ref": "refs/heads/main",
                "installation": { "id": 123 },
                "repository": {
                    "id": 42,
                    "full_name": "owner/repo"
                }
            }),
        };

        let ack = process_event(&runtime, &event)
            .await
            .expect("process event");

        assert_eq!(RelayAckStatus::Processed, ack);
        assert_eq!(vec![project_id], *runtime.push_events.lock().await);
        assert_eq!(0, runtime.relay_requests.lock().await.len());
    }

    #[tokio::test]
    async fn completed_check_run_event_queues_associated_prs() {
        let project_id = Uuid::new_v4();
        let runtime = FakeRelayRuntime::new(project_id);
        let event = RelayEvent {
            sequence: 1,
            delivery_id: "delivery-3".to_string(),
            kind: RelayEventKind::CheckRun,
            payload: json!({
                "action": "completed",
                "installation": { "id": 123 },
                "repository": {
                    "id": 42,
                    "full_name": "owner/repo"
                },
                "check_run": {
                    "head_sha": "head-21",
                    "pull_requests": [
                        { "number": 21 },
                        { "number": 22 }
                    ]
                }
            }),
        };

        let ack = process_event(&runtime, &event)
            .await
            .expect("process event");

        assert_eq!(RelayAckStatus::Processed, ack);
        let requests = runtime.relay_requests.lock().await;
        let mut prs = requests
            .iter()
            .map(|request| (request.pr, request.head_sha.as_deref()))
            .collect::<Vec<_>>();
        prs.sort_by_key(|(pr, _)| *pr);
        assert_eq!(vec![(21, Some("head-21")), (22, Some("head-21"))], prs);
    }
}
