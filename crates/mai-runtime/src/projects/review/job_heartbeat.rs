use chrono::{TimeDelta, Utc};
use mai_protocol::ProjectReviewJobSummary;
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

use super::worker::ProjectReviewWorkerOps;

const REVIEW_JOB_HEARTBEAT_OPERATION_TIMEOUT: Duration =
    Duration::from_secs(mai_store::REVIEW_JOB_SQLITE_BUSY_TIMEOUT_SECS + 5);
const REVIEW_JOB_HEARTBEAT_RETRY_DELAY: Duration = Duration::from_secs(1);

pub(super) async fn run_project_review_job_heartbeat(
    ops: impl ProjectReviewWorkerOps,
    project_id: mai_protocol::ProjectId,
    job_id: uuid::Uuid,
    owner: String,
    cancellation_token: CancellationToken,
    attempt_cancellation_token: CancellationToken,
) {
    let mut completed_heartbeats = 0;
    loop {
        tokio::select! {
            _ = sleep(heartbeat_delay(completed_heartbeats)) => {
                let current_time = Utc::now();
                let heartbeat = wait_heartbeat_operation(
                    &cancellation_token,
                    ops.heartbeat_project_review_job(
                        job_id,
                        owner.clone(),
                        current_time,
                        current_time + TimeDelta::seconds(super::job::REVIEW_JOB_LEASE_SECONDS),
                    ),
                    REVIEW_JOB_HEARTBEAT_OPERATION_TIMEOUT,
                ).await;
                match heartbeat {
                    HeartbeatOperation::Completed(Ok(true)) => {
                        completed_heartbeats += 1;
                    }
                    HeartbeatOperation::Completed(Ok(false)) => {
                        let current = wait_heartbeat_operation(
                            &cancellation_token,
                            ops.load_project_review_job(project_id, job_id),
                            REVIEW_JOB_HEARTBEAT_OPERATION_TIMEOUT,
                        ).await;
                        match current {
                            HeartbeatOperation::Completed(current) => {
                                if let Ok(Some(current)) = current.as_ref() {
                                    tracing::warn!(
                                        job_id = %job_id,
                                        status = %current.status,
                                        lease_owner = ?current.lease_owner,
                                        lease_expires_at = ?current.lease_expires_at,
                                        "review job heartbeat lost its lease"
                                    );
                                }
                                if lease_loss_requires_attempt_cancellation(
                                    current.as_ref().ok().and_then(Option::as_ref),
                                ) {
                                    attempt_cancellation_token.cancel();
                                }
                            }
                            HeartbeatOperation::TimedOut => {
                                tracing::warn!(job_id = %job_id, "timed out while confirming lost review job lease");
                                attempt_cancellation_token.cancel();
                            }
                            HeartbeatOperation::Cancelled => {}
                        }
                        break;
                    }
                    HeartbeatOperation::Completed(Err(error)) => {
                        if heartbeat_error_requires_attempt_cancellation(&error) {
                            tracing::warn!(job_id = %job_id, "review job heartbeat failed: {error}");
                            attempt_cancellation_token.cancel();
                            break;
                        }
                        tracing::warn!(
                            job_id = %job_id,
                            retry_delay_ms = REVIEW_JOB_HEARTBEAT_RETRY_DELAY.as_millis(),
                            "review job heartbeat hit temporary SQLite contention; retrying"
                        );
                        completed_heartbeats = 0;
                        if !wait_heartbeat_retry(&cancellation_token).await {
                            break;
                        }
                    }
                    HeartbeatOperation::TimedOut => {
                        tracing::warn!(
                            job_id = %job_id,
                            timeout_seconds = REVIEW_JOB_HEARTBEAT_OPERATION_TIMEOUT.as_secs(),
                            retry_delay_ms = REVIEW_JOB_HEARTBEAT_RETRY_DELAY.as_millis(),
                            "review job heartbeat operation timed out; retrying"
                        );
                        completed_heartbeats = 0;
                        if !wait_heartbeat_retry(&cancellation_token).await {
                            break;
                        }
                    }
                    HeartbeatOperation::Cancelled => break,
                }
            }
            _ = cancellation_token.cancelled() => break,
        }
    }
}

fn heartbeat_delay(completed_heartbeats: u64) -> Duration {
    if completed_heartbeats == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs(super::job::REVIEW_JOB_HEARTBEAT_SECONDS)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HeartbeatOperation<T> {
    Completed(T),
    Cancelled,
    TimedOut,
}

async fn wait_heartbeat_operation<T>(
    cancellation_token: &CancellationToken,
    operation: impl std::future::Future<Output = T>,
    operation_timeout: Duration,
) -> HeartbeatOperation<T> {
    tokio::select! {
        _ = cancellation_token.cancelled() => HeartbeatOperation::Cancelled,
        result = tokio::time::timeout(operation_timeout, operation) => {
            match result {
                Ok(result) => HeartbeatOperation::Completed(result),
                Err(_) => HeartbeatOperation::TimedOut,
            }
        }
    }
}

async fn wait_heartbeat_retry(cancellation_token: &CancellationToken) -> bool {
    tokio::select! {
        _ = sleep(REVIEW_JOB_HEARTBEAT_RETRY_DELAY) => true,
        _ = cancellation_token.cancelled() => false,
    }
}

fn lease_loss_requires_attempt_cancellation(current: Option<&ProjectReviewJobSummary>) -> bool {
    current.is_none_or(|job| job.submission_receipt.is_none())
}

fn heartbeat_error_requires_attempt_cancellation(error: &crate::RuntimeError) -> bool {
    if let crate::RuntimeError::Store(error) = error {
        return !mai_store::is_retryable_sqlite_error(error);
    }
    true
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn heartbeat_operation_honors_timeout_and_shutdown() {
        let active = CancellationToken::new();
        assert_eq!(
            HeartbeatOperation::<()>::TimedOut,
            wait_heartbeat_operation(&active, std::future::pending(), Duration::from_millis(10),)
                .await
        );

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert_eq!(
            HeartbeatOperation::<()>::Cancelled,
            wait_heartbeat_operation(&cancelled, std::future::pending(), Duration::from_secs(1),)
                .await
        );
    }

    #[test]
    fn heartbeat_renews_immediately_then_keeps_a_safe_steady_interval() {
        assert_eq!(Duration::ZERO, heartbeat_delay(0));
        assert_eq!(
            Duration::from_secs(super::super::job::REVIEW_JOB_HEARTBEAT_SECONDS),
            heartbeat_delay(1)
        );
        assert!(
            REVIEW_JOB_HEARTBEAT_OPERATION_TIMEOUT.as_secs()
                > mai_store::REVIEW_JOB_SQLITE_BUSY_TIMEOUT_SECS
        );
        assert!(
            super::super::job::REVIEW_JOB_HEARTBEAT_SECONDS
                + 2 * (REVIEW_JOB_HEARTBEAT_OPERATION_TIMEOUT.as_secs()
                    + REVIEW_JOB_HEARTBEAT_RETRY_DELAY.as_secs())
                < super::super::job::REVIEW_JOB_LEASE_SECONDS as u64
        );
    }

    #[test]
    fn persisted_submission_receipt_prevents_turn_cancellation_after_lease_loss() {
        let mut job =
            super::super::job::new_project_review_job(super::super::job::NewProjectReviewJob {
                project_id: uuid::Uuid::new_v4(),
                pr: 42,
                head_sha: "head".to_string(),
                source: mai_protocol::ProjectReviewJobSource::Manual,
                delivery_id: None,
                reason: "test".to_string(),
            });
        assert!(lease_loss_requires_attempt_cancellation(Some(&job)));

        job.submission_receipt = Some(mai_protocol::ProjectReviewSubmissionReceipt {
            github_review_id: 42,
            event: mai_protocol::ProjectReviewDecision::Approve,
            head_sha: job.head_sha.clone(),
            html_url: None,
            submitted_at: Utc::now(),
        });

        assert!(!lease_loss_requires_attempt_cancellation(Some(&job)));
        assert!(lease_loss_requires_attempt_cancellation(None));
    }

    #[test]
    fn sqlite_busy_heartbeat_keeps_attempt_alive_for_retry() {
        let error = crate::RuntimeError::Store(mai_store::StoreError::Sqlite(
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                None,
            ),
        ));

        assert!(!heartbeat_error_requires_attempt_cancellation(&error));
    }
}
