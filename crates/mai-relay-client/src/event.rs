use mai_protocol::{ProjectId, RelayAckStatus, RelayEvent, RelayEventKind, ServiceEventKind};
use mai_runtime::RuntimeError;
use serde_json::Value;

use crate::client::RelayClient;
use crate::protocol;

pub(crate) async fn process_event(
    client: &RelayClient,
    event: &RelayEvent,
) -> Result<RelayAckStatus, RuntimeError> {
    let runtime =
        client.runtime.lock().await.clone().ok_or_else(|| {
            RuntimeError::InvalidInput("relay runtime is not attached".to_string())
        })?;
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

    let Some(project_id) = client
        .runtime
        .lock()
        .await
        .clone()
        .ok_or_else(|| RuntimeError::InvalidInput("relay runtime is not attached".to_string()))?
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
            let Some(pr) = event
                .payload
                .get("pull_request")
                .and_then(|pr| pr.get("number"))
                .and_then(Value::as_u64)
                .or_else(|| event.payload.get("number").and_then(Value::as_u64))
            else {
                return Ok(RelayAckStatus::Ignored);
            };
            queue_review(client, project_id, &event.delivery_id, pr, &event_name).await?;
            Ok(RelayAckStatus::Processed)
        }
        RelayEventKind::CheckRun | RelayEventKind::CheckSuite => {
            if action.as_deref() != Some("completed") {
                return Ok(RelayAckStatus::Ignored);
            }
            let prs = protocol::associated_pull_requests(&event.payload);
            if prs.is_empty() {
                return Ok(RelayAckStatus::Ignored);
            }
            let mut processed = false;
            for pr in prs {
                queue_review(client, project_id, &event.delivery_id, pr, &event_name).await?;
                processed = true;
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

async fn queue_review(
    client: &RelayClient,
    project_id: ProjectId,
    delivery_id: &str,
    pr: u64,
    reason: &str,
) -> Result<(), RuntimeError> {
    let runtime =
        client.runtime.lock().await.clone().ok_or_else(|| {
            RuntimeError::InvalidInput("relay runtime is not attached".to_string())
        })?;
    runtime
        .publish_external_event(ServiceEventKind::ProjectReviewQueued {
            project_id,
            delivery_id: delivery_id.to_string(),
            pr,
            reason: reason.to_string(),
        })
        .await;
    runtime
        .trigger_project_review(project_id, pr, delivery_id.to_string(), reason.to_string())
        .await
}
