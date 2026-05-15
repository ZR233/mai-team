use std::sync::Arc;

use mai_protocol::{RelayAckStatus, RelayEvent, RelayEventKind, ServiceEventKind};
use mai_relay_client::RelayClient;
use mai_runtime::{AgentRuntime, ProjectReviewQueueRequest, RuntimeError};
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

async fn process_event(
    runtime: &Arc<AgentRuntime>,
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
                .enqueue_project_review(ProjectReviewQueueRequest {
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
                    .enqueue_project_review(ProjectReviewQueueRequest {
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
