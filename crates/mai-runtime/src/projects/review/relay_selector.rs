use std::sync::Arc;

use mai_protocol::{ProjectCloneStatus, ProjectId, ProjectStatus, ProjectSummary};
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

use super::eligibility::SelectedProjectReviewPr;
use super::project_review_retry_backoff;
use super::relay_queue::PendingProjectReviewRelaySignal;
use super::worker::ProjectReviewWorkerOps;
use crate::{Result, RuntimeError};

pub(crate) async fn run_project_review_relay_selector_loop(
    ops: impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    cancellation_token: CancellationToken,
) {
    let mut retry_backoff = project_review_retry_backoff();
    loop {
        if cancellation_token.is_cancelled() || !project_still_ready(&ops, project_id).await {
            break;
        }

        let signal = match next_project_review_relay_signal(&ops, project_id).await {
            Ok(Some(signal)) => signal,
            Ok(None) => {
                wait_for_project_review_relay_signal(&ops, project_id, &cancellation_token).await;
                continue;
            }
            Err(err) => {
                tracing::warn!(project_id = %project_id, "failed to read project relay review queue: {err}");
                if !wait_or_cancel(&cancellation_token, Duration::from_secs(1)).await {
                    break;
                }
                continue;
            }
        };

        let selected = match ops
            .select_project_review_pr(project_id, signal.pr, signal.head_sha.clone())
            .await
        {
            Ok(Some(selected)) => selected,
            Ok(None) => {
                tracing::debug!(
                    project_id = %project_id,
                    pr = signal.pr,
                    "relay PR signal did not satisfy review eligibility"
                );
                retry_backoff.reset();
                continue;
            }
            Err(err) => {
                let retry_delay = retry_backoff.next_delay();
                tracing::warn!(
                    project_id = %project_id,
                    pr = signal.pr,
                    "failed to evaluate relay PR signal; requeueing relay signal: {err}"
                );
                requeue_project_review_relay_signal(&ops, project_id, signal).await;
                if !wait_for_relay_retry(&cancellation_token, retry_delay).await {
                    break;
                }
                continue;
            }
        };

        if let Err(err) =
            enqueue_selected_relay_signal(&ops, project_id, signal.clone(), selected).await
        {
            let retry_delay = retry_backoff.next_delay();
            tracing::warn!(
                project_id = %project_id,
                "failed to enqueue eligible relay PR signal; requeueing relay signal: {err}"
            );
            requeue_project_review_relay_signal(&ops, project_id, signal).await;
            if !wait_for_relay_retry(&cancellation_token, retry_delay).await {
                break;
            }
            continue;
        }
        retry_backoff.reset();
    }
}

async fn enqueue_selected_relay_signal(
    ops: &impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    relay_signal: PendingProjectReviewRelaySignal,
    selected: SelectedProjectReviewPr,
) -> Result<()> {
    let head_sha = selected.head_sha.or(relay_signal.head_sha);
    ops.enqueue_project_review_signals(
        project_id,
        vec![crate::projects::review::pool::ProjectReviewSignalInput {
            pr: selected.pr,
            head_sha,
            delivery_id: relay_signal.delivery_id,
            reason: relay_signal.reason,
        }],
    )
    .await?;
    Ok(())
}

async fn next_project_review_relay_signal(
    ops: &impl ProjectReviewWorkerOps,
    project_id: ProjectId,
) -> Result<Option<PendingProjectReviewRelaySignal>> {
    let project = ops.project(project_id).await?;
    Ok(project.relay_review_queue.lock().await.next())
}

async fn requeue_project_review_relay_signal(
    ops: &impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    signal: PendingProjectReviewRelaySignal,
) {
    if let Ok(project) = ops.project(project_id).await {
        project.relay_review_queue.lock().await.requeue(signal);
        project.relay_review_notify.notify_waiters();
    }
}

async fn wait_for_project_review_relay_signal(
    ops: &impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    cancellation_token: &CancellationToken,
) {
    let notify = match ops.project(project_id).await {
        Ok(project) => Arc::clone(&project.relay_review_notify),
        Err(err) => {
            tracing::warn!(project_id = %project_id, "failed to wait for relay review signal: {err}");
            return;
        }
    };
    tokio::select! {
        _ = notify.notified() => {}
        _ = sleep(Duration::from_secs(1)) => {}
        _ = cancellation_token.cancelled() => {}
    }
}

async fn wait_for_relay_retry(cancellation_token: &CancellationToken, delay: Duration) -> bool {
    wait_or_cancel(cancellation_token, delay).await
}

async fn project_still_ready(ops: &impl ProjectReviewWorkerOps, project_id: ProjectId) -> bool {
    match ops.project(project_id).await {
        Ok(project) => {
            let summary = project.summary.read().await;
            project_ready_for_review(&summary)
        }
        Err(RuntimeError::AgentNotFound(_))
        | Err(RuntimeError::TaskNotFound(_))
        | Err(RuntimeError::ProjectNotFound(_))
        | Err(RuntimeError::ProjectReviewRunNotFound(_))
        | Err(RuntimeError::AgentBusy(_))
        | Err(RuntimeError::TaskBusy(_))
        | Err(RuntimeError::MissingContainer(_))
        | Err(RuntimeError::SessionNotFound { .. })
        | Err(RuntimeError::ToolTraceNotFound { .. })
        | Err(RuntimeError::TurnNotFound { .. })
        | Err(RuntimeError::TurnCancelled)
        | Err(RuntimeError::Docker(_))
        | Err(RuntimeError::Model(_))
        | Err(RuntimeError::Mcp(_))
        | Err(RuntimeError::Store(_))
        | Err(RuntimeError::Skill(_))
        | Err(RuntimeError::InvalidInput(_))
        | Err(RuntimeError::Io(_))
        | Err(RuntimeError::Http(_))
        | Err(RuntimeError::Jwt(_)) => false,
    }
}

async fn wait_or_cancel(cancellation_token: &CancellationToken, delay: Duration) -> bool {
    if delay.is_zero() {
        return true;
    }
    tokio::select! {
        _ = sleep(delay) => true,
        _ = cancellation_token.cancelled() => false,
    }
}

fn project_ready_for_review(summary: &ProjectSummary) -> bool {
    summary.auto_review_enabled
        && summary.status == ProjectStatus::Ready
        && summary.clone_status == ProjectCloneStatus::Ready
}
