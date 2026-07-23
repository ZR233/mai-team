use chrono::{TimeDelta, Utc};
use mai_protocol::{
    ProjectReviewFailure, ProjectReviewFailureCategory, ProjectReviewJobSource,
    ProjectReviewJobSummary, ProjectReviewOutcome,
};
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

use crate::RuntimeError;

use super::state::{ReviewStateUpdate, ReviewerAgentUpdate};
use super::worker::{ProjectReviewTaskContext, ProjectReviewWorkerOps};

pub(super) async fn run_claimed_project_review_job(
    ops: &ProjectReviewTaskContext<impl ProjectReviewWorkerOps>,
    job: ProjectReviewJobSummary,
    owner: String,
) -> bool {
    project_job_state_projection(ops, &job, None, None).await;
    if job.status == mai_protocol::ProjectReviewJobStatus::Reconciling {
        let heartbeat_cancel = CancellationToken::new();
        let reconciliation_cancel = ops.cancellation_token.child_token();
        let heartbeat = tokio::spawn(run_project_review_job_heartbeat(
            ops.ops.clone(),
            ops.project_id,
            job.id,
            owner.clone(),
            heartbeat_cancel.clone(),
            reconciliation_cancel.clone(),
        ));
        let result = tokio::select! {
            result = run_claimed_project_review_reconciliation(ops, job, owner) => result,
            _ = reconciliation_cancel.cancelled() => !ops.cancellation_token.is_cancelled(),
        };
        heartbeat_cancel.cancel();
        let _ = heartbeat.await;
        return result;
    }
    let job = match preflight_project_review_job(ops, job, &owner).await {
        ProjectReviewPreflight::Continue(job) => *job,
        ProjectReviewPreflight::Finished => return true,
        ProjectReviewPreflight::Cancelled => return false,
    };
    let heartbeat_cancel = CancellationToken::new();
    let attempt_cancel = ops.cancellation_token.child_token();
    let heartbeat = tokio::spawn(run_project_review_job_heartbeat(
        ops.ops.clone(),
        ops.project_id,
        job.id,
        owner.clone(),
        heartbeat_cancel.clone(),
        attempt_cancel.clone(),
    ));
    let attempt = ops
        .ops
        .run_project_review_job_attempt(job.clone(), owner.clone(), attempt_cancel)
        .await;
    heartbeat_cancel.cancel();
    let _ = heartbeat.await;

    if ops.cancellation_token.is_cancelled() {
        return false;
    }
    let mut current = match ops
        .ops
        .load_project_review_job(ops.project_id, job.id)
        .await
    {
        Ok(Some(job)) => job,
        Ok(None) => {
            tracing::error!(job_id = %job.id, "claimed project review job disappeared");
            return true;
        }
        Err(error) => {
            tracing::warn!(job_id = %job.id, "failed to reload claimed project review job: {error}");
            return true;
        }
    };
    if current.submission_receipt.is_some() {
        cleanup_terminal_reviewer(ops, &current).await;
        project_job_state_projection(
            ops,
            &current,
            Some(ProjectReviewOutcome::ReviewSubmitted),
            None,
        )
        .await;
        return true;
    }
    if current.status.is_terminal() {
        cleanup_terminal_reviewer(ops, &current).await;
        release_terminal_lease(ops, &mut current, &owner).await;
        project_job_state_projection(ops, &current, None, None).await;
        return true;
    }
    if current.status == mai_protocol::ProjectReviewJobStatus::Reconciling {
        schedule_review_reconciliation(&mut current, None);
        let _ = ops
            .ops
            .save_claimed_project_review_job(current.clone(), owner)
            .await;
        project_job_state_projection(ops, &current, None, None).await;
        return true;
    }

    let (outcome, summary) = match attempt {
        Ok(result) => apply_review_cycle_result(&mut current, result),
        Err(crate::RuntimeError::TurnCancelled) => {
            apply_review_failure(
                &mut current,
                ProjectReviewFailure {
                    category: ProjectReviewFailureCategory::Internal,
                    code: Some("attempt_interrupted".to_string()),
                    http_status: None,
                    message: "review attempt was interrupted before completion".to_string(),
                    retry: pl_protocol::RetryDisposition::Retryable {
                        retry_after_ms: None,
                    },
                },
            );
            (Some(ProjectReviewOutcome::Failed), None)
        }
        Err(error) => {
            let message = error.to_string();
            if message.contains(super::REVIEW_TARGET_HEAD_CHANGED) {
                super::job::supersede_job(&mut current, Utc::now());
            } else {
                let failure = runtime_failure(&error);
                apply_review_failure(&mut current, failure);
            }
            (Some(ProjectReviewOutcome::Failed), None)
        }
    };
    let saved = ops
        .ops
        .save_claimed_project_review_job(current.clone(), owner)
        .await;
    match saved {
        Ok(true) => {}
        Ok(false) => {
            if let Ok(Some(reloaded)) = ops
                .ops
                .load_project_review_job(ops.project_id, current.id)
                .await
            {
                current = reloaded;
            }
        }
        Err(error) => {
            tracing::warn!(job_id = %current.id, "failed to persist project review job transition: {error}");
            return true;
        }
    }
    if current.status.is_terminal() {
        cleanup_terminal_reviewer(ops, &current).await;
    }
    project_job_state_projection(ops, &current, outcome, summary).await;
    true
}

enum ProjectReviewPreflight {
    Continue(Box<ProjectReviewJobSummary>),
    Finished,
    Cancelled,
}

async fn preflight_project_review_job(
    ops: &ProjectReviewTaskContext<impl ProjectReviewWorkerOps>,
    mut job: ProjectReviewJobSummary,
    owner: &str,
) -> ProjectReviewPreflight {
    if job.source == ProjectReviewJobSource::Manual {
        return ProjectReviewPreflight::Continue(Box::new(job));
    }
    if ops.cancellation_token.is_cancelled() {
        return ProjectReviewPreflight::Cancelled;
    }
    let evaluated = match ops
        .ops
        .evaluate_project_review_pr(job.project_id, job.pr, Some(job.head_sha.clone()))
        .await
    {
        Ok(evaluated) => evaluated,
        Err(error) => {
            let failure = runtime_failure(&error);
            apply_review_failure(&mut job, failure);
            let _ = ops
                .ops
                .save_claimed_project_review_job(job.clone(), owner.to_string())
                .await;
            project_job_state_projection(ops, &job, Some(ProjectReviewOutcome::Failed), None).await;
            tracing::warn!(
                job_id = %job.id,
                pr = job.pr,
                error = %error,
                "project review final eligibility check failed; scheduled retry"
            );
            return ProjectReviewPreflight::Finished;
        }
    };
    let Some(current_head) = evaluated.head_sha.clone() else {
        let error = RuntimeError::InvalidInput(
            "GitHub pull request response is missing the current head SHA".to_string(),
        );
        apply_review_failure(&mut job, runtime_failure(&error));
        let _ = ops
            .ops
            .save_claimed_project_review_job(job.clone(), owner.to_string())
            .await;
        project_job_state_projection(ops, &job, Some(ProjectReviewOutcome::Failed), None).await;
        return ProjectReviewPreflight::Finished;
    };
    if current_head != job.head_sha {
        super::job::supersede_job(&mut job, Utc::now());
        let saved = ops
            .ops
            .save_claimed_project_review_job(job.clone(), owner.to_string())
            .await;
        if !matches!(saved, Ok(true)) {
            tracing::warn!(
                job_id = %job.id,
                pr = job.pr,
                "failed to persist superseded review job during final eligibility check"
            );
            return ProjectReviewPreflight::Finished;
        }
        if evaluated.skip_reason.is_none()
            && let Err(error) = ops
                .ops
                .enqueue_project_review_replacement(job.clone(), current_head.clone())
                .await
        {
            tracing::warn!(
                job_id = %job.id,
                pr = job.pr,
                head_sha = %current_head,
                error = %error,
                "failed to enqueue replacement review job for the current head"
            );
        }
        project_job_state_projection(ops, &job, None, None).await;
        tracing::info!(
            job_id = %job.id,
            pr = job.pr,
            old_head_sha = %job.head_sha,
            current_head_sha = %current_head,
            "superseded stale review job during final eligibility check"
        );
        return ProjectReviewPreflight::Finished;
    }
    if let Some(reason) = evaluated.skip_reason {
        super::job::skip_job(&mut job, reason.clone(), Utc::now());
        let saved = ops
            .ops
            .save_claimed_project_review_job(job.clone(), owner.to_string())
            .await;
        if !matches!(saved, Ok(true)) {
            tracing::warn!(
                job_id = %job.id,
                pr = job.pr,
                "failed to persist skipped review job during final eligibility check"
            );
        }
        project_job_state_projection(ops, &job, None, None).await;
        tracing::info!(
            job_id = %job.id,
            pr = job.pr,
            skip_reason = %reason,
            "skipped review job during final eligibility check"
        );
        return ProjectReviewPreflight::Finished;
    }
    ProjectReviewPreflight::Continue(Box::new(job))
}

fn apply_review_cycle_result(
    job: &mut ProjectReviewJobSummary,
    result: super::ProjectReviewCycleResult,
) -> (Option<ProjectReviewOutcome>, Option<String>) {
    let outcome = Some(result.outcome.clone());
    let summary = result.summary.clone();
    match result.outcome {
        ProjectReviewOutcome::ReviewSubmitted if job.submission_receipt.is_some() => {
            super::job::succeed_job(job, Utc::now());
        }
        ProjectReviewOutcome::ReviewSubmitted => {
            super::job::fail_job(
                job,
                ProjectReviewFailure {
                    category: ProjectReviewFailureCategory::Validation,
                    code: Some("missing_submission_receipt".to_string()),
                    http_status: None,
                    message:
                        "reviewer reported a submitted review without a persisted GitHub receipt"
                            .to_string(),
                    retry: pl_protocol::RetryDisposition::Permanent,
                },
                Utc::now(),
            );
        }
        ProjectReviewOutcome::NoEligiblePr => {
            super::job::succeed_job(job, Utc::now());
        }
        ProjectReviewOutcome::Failed => {
            let failure = result.failure.unwrap_or_else(|| {
                permanent_failure(
                    ProjectReviewFailureCategory::Validation,
                    result
                        .error
                        .unwrap_or_else(|| "reviewer reported failure".to_string()),
                )
            });
            apply_review_failure(job, failure);
        }
    }
    (outcome, summary)
}

async fn run_claimed_project_review_reconciliation(
    ops: &ProjectReviewTaskContext<impl ProjectReviewWorkerOps>,
    mut job: ProjectReviewJobSummary,
    owner: String,
) -> bool {
    let result = ops.ops.reconcile_project_review_job(job.clone()).await;
    match result {
        Ok(Some(_receipt)) => {
            if let Ok(Some(current)) = ops
                .ops
                .load_project_review_job(ops.project_id, job.id)
                .await
            {
                cleanup_terminal_reviewer(ops, &current).await;
                project_job_state_projection(
                    ops,
                    &current,
                    Some(ProjectReviewOutcome::ReviewSubmitted),
                    None,
                )
                .await;
            }
        }
        Ok(None) => {
            schedule_review_reconciliation(&mut job, None);
            let _ = ops
                .ops
                .save_claimed_project_review_job(job.clone(), owner)
                .await;
            if job.status.is_terminal() {
                cleanup_terminal_reviewer(ops, &job).await;
            }
            project_job_state_projection(ops, &job, None, None).await;
        }
        Err(error) => {
            schedule_review_reconciliation(&mut job, Some(error.to_string()));
            let _ = ops
                .ops
                .save_claimed_project_review_job(job.clone(), owner)
                .await;
            if job.status.is_terminal() {
                cleanup_terminal_reviewer(ops, &job).await;
            }
            project_job_state_projection(ops, &job, None, None).await;
        }
    }
    true
}

fn schedule_review_reconciliation(job: &mut ProjectReviewJobSummary, error: Option<String>) {
    let current_time = Utc::now();
    let deadline = job
        .submission_intent
        .as_ref()
        .map(|intent| intent.created_at + TimeDelta::minutes(5))
        .unwrap_or(current_time);
    if current_time >= deadline {
        super::job::fail_job(
            job,
            permanent_failure(
                ProjectReviewFailureCategory::Github,
                error.unwrap_or_else(|| {
                    "GitHub review submission could not be reconciled within five minutes"
                        .to_string()
                }),
            ),
            current_time,
        );
        return;
    }
    job.status = mai_protocol::ProjectReviewJobStatus::Reconciling;
    job.next_attempt_at = Some((current_time + TimeDelta::seconds(10)).min(deadline));
    job.active_run_id = None;
    job.lease_owner = None;
    job.lease_expires_at = None;
    job.updated_at = current_time;
    if let Some(error) = error {
        job.failure = Some(ProjectReviewFailure {
            category: ProjectReviewFailureCategory::Github,
            code: None,
            http_status: None,
            message: error,
            retry: pl_protocol::RetryDisposition::Retryable {
                retry_after_ms: Some(10_000),
            },
        });
    }
}

async fn run_project_review_job_heartbeat(
    ops: impl ProjectReviewWorkerOps,
    project_id: mai_protocol::ProjectId,
    job_id: uuid::Uuid,
    owner: String,
    cancellation_token: CancellationToken,
    attempt_cancellation_token: CancellationToken,
) {
    let delay = Duration::from_secs(super::job::REVIEW_JOB_HEARTBEAT_SECONDS);
    loop {
        tokio::select! {
            _ = sleep(delay) => {
                let current_time = Utc::now();
                match ops.heartbeat_project_review_job(
                    job_id,
                    owner.clone(),
                    current_time,
                    current_time + TimeDelta::seconds(super::job::REVIEW_JOB_LEASE_SECONDS),
                ).await {
                    Ok(true) => {}
                    Ok(false) => {
                        let current = ops.load_project_review_job(project_id, job_id).await;
                        if heartbeat_lease_loss_requires_attempt_cancellation(
                            current.as_ref().ok().and_then(Option::as_ref),
                        ) {
                            attempt_cancellation_token.cancel();
                        }
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(job_id = %job_id, "review job heartbeat failed: {error}");
                        attempt_cancellation_token.cancel();
                        break;
                    }
                }
            }
            _ = cancellation_token.cancelled() => break,
        }
    }
}

fn heartbeat_lease_loss_requires_attempt_cancellation(
    current: Option<&ProjectReviewJobSummary>,
) -> bool {
    current.is_none_or(|job| job.submission_receipt.is_none())
}

async fn release_terminal_lease(
    ops: &ProjectReviewTaskContext<impl ProjectReviewWorkerOps>,
    job: &mut ProjectReviewJobSummary,
    owner: &str,
) {
    if job.lease_owner.as_deref() != Some(owner) {
        return;
    }
    job.lease_owner = None;
    job.lease_expires_at = None;
    job.updated_at = Utc::now();
    let _ = ops
        .ops
        .save_claimed_project_review_job(job.clone(), owner.to_string())
        .await;
}

fn apply_review_failure(job: &mut ProjectReviewJobSummary, failure: ProjectReviewFailure) {
    if failure.retry.is_retryable() {
        super::job::schedule_retry(job, failure, Utc::now());
    } else {
        super::job::fail_job(job, failure, Utc::now());
    }
}

pub(super) fn runtime_failure(error: &RuntimeError) -> ProjectReviewFailure {
    let (category, code, http_status, retry) = match error {
        RuntimeError::GithubUnavailable {
            status,
            retry_after,
            ..
        } => (
            ProjectReviewFailureCategory::Github,
            Some("github_unavailable".to_string()),
            Some(status.as_u16()),
            retryable_disposition(retry_after.map(duration_millis)),
        ),
        RuntimeError::Http(error)
            if error.is_timeout()
                || error.is_connect()
                || error.is_request()
                || error.is_body() =>
        {
            (
                ProjectReviewFailureCategory::Github,
                Some("http_transport".to_string()),
                error.status().map(|status| status.as_u16()),
                retryable_disposition(None),
            )
        }
        RuntimeError::Docker(error) => match error {
            mai_docker::DockerError::NotAvailable(_)
            | mai_docker::DockerError::CommandFailed(_)
            | mai_docker::DockerError::Io(_)
            | mai_docker::DockerError::Cancelled => (
                ProjectReviewFailureCategory::Workspace,
                Some("docker_unavailable".to_string()),
                None,
                retryable_disposition(None),
            ),
            mai_docker::DockerError::Utf8(_)
            | mai_docker::DockerError::Json(_)
            | mai_docker::DockerError::InvalidImage(_)
            | mai_docker::DockerError::InvalidMount(_) => (
                ProjectReviewFailureCategory::Validation,
                Some("invalid_workspace_configuration".to_string()),
                None,
                pl_protocol::RetryDisposition::Permanent,
            ),
        },
        RuntimeError::Model(error) => model_runtime_failure(error),
        RuntimeError::Store(_) => (
            ProjectReviewFailureCategory::Internal,
            Some("store_unavailable".to_string()),
            None,
            retryable_disposition(None),
        ),
        RuntimeError::Io(error) if io_error_is_retryable(error.kind()) => (
            ProjectReviewFailureCategory::Workspace,
            Some("workspace_io_unavailable".to_string()),
            None,
            retryable_disposition(None),
        ),
        RuntimeError::AgentBusy(_) | RuntimeError::TaskBusy(_) => (
            ProjectReviewFailureCategory::Internal,
            Some("runtime_busy".to_string()),
            None,
            retryable_disposition(None),
        ),
        RuntimeError::MissingContainer(_) => (
            ProjectReviewFailureCategory::Workspace,
            Some("workspace_container_missing".to_string()),
            None,
            retryable_disposition(None),
        ),
        RuntimeError::TurnCancelled => (
            ProjectReviewFailureCategory::Internal,
            Some("attempt_interrupted".to_string()),
            None,
            retryable_disposition(None),
        ),
        RuntimeError::InvalidInput(message)
            if super::project_review_error_is_retryable(message) =>
        {
            (
                ProjectReviewFailureCategory::Github,
                Some("transient_integration_error".to_string()),
                None,
                retryable_disposition(None),
            )
        }
        RuntimeError::AgentNotFound(_)
        | RuntimeError::TaskNotFound(_)
        | RuntimeError::ProjectNotFound(_)
        | RuntimeError::ProjectReviewRunNotFound(_)
        | RuntimeError::ProjectReviewJobNotFound(_)
        | RuntimeError::SessionNotFound { .. }
        | RuntimeError::SessionEventNotFound(_)
        | RuntimeError::ToolTraceNotFound { .. }
        | RuntimeError::TurnNotFound { .. }
        | RuntimeError::Skill(_)
        | RuntimeError::InvalidInput(_)
        | RuntimeError::Io(_)
        | RuntimeError::Http(_)
        | RuntimeError::Jwt(_) => (
            ProjectReviewFailureCategory::Validation,
            Some("permanent_runtime_error".to_string()),
            None,
            pl_protocol::RetryDisposition::Permanent,
        ),
    };
    ProjectReviewFailure {
        category,
        code,
        http_status,
        message: error.to_string(),
        retry,
    }
}

fn model_runtime_failure(
    error: &pl_protocol::PureError,
) -> (
    ProjectReviewFailureCategory,
    Option<String>,
    Option<u16>,
    pl_protocol::RetryDisposition,
) {
    match error {
        pl_protocol::PureError::ProviderCapacity { .. } => (
            ProjectReviewFailureCategory::ProviderCapacity,
            Some("provider_capacity".to_string()),
            None,
            retryable_disposition(None),
        ),
        pl_protocol::PureError::TransientModelTransport {
            retry_after_ms,
            code,
            http_status,
            ..
        } => (
            ProjectReviewFailureCategory::Provider,
            code.clone(),
            *http_status,
            retryable_disposition(*retry_after_ms),
        ),
        pl_protocol::PureError::Io(_) | pl_protocol::PureError::HttpError(_) => (
            ProjectReviewFailureCategory::Provider,
            Some("provider_transport".to_string()),
            None,
            retryable_disposition(None),
        ),
        pl_protocol::PureError::LlmError(_)
        | pl_protocol::PureError::ContextOverflow(_)
        | pl_protocol::PureError::ToolNotFound(_)
        | pl_protocol::PureError::ToolExecutionFailed { .. }
        | pl_protocol::PureError::AgentLimitReached { .. }
        | pl_protocol::PureError::AgentDepthLimitReached { .. }
        | pl_protocol::PureError::PermissionDenied(_)
        | pl_protocol::PureError::SandboxError(_)
        | pl_protocol::PureError::MemoryError(_)
        | pl_protocol::PureError::ConfigError(_)
        | pl_protocol::PureError::SerdeJson(_) => (
            ProjectReviewFailureCategory::Validation,
            Some("permanent_model_error".to_string()),
            None,
            pl_protocol::RetryDisposition::Permanent,
        ),
    }
}

fn retryable_disposition(retry_after_ms: Option<u64>) -> pl_protocol::RetryDisposition {
    pl_protocol::RetryDisposition::Retryable { retry_after_ms }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn io_error_is_retryable(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::BrokenPipe
    )
}

fn permanent_failure(
    category: ProjectReviewFailureCategory,
    message: String,
) -> ProjectReviewFailure {
    ProjectReviewFailure {
        category,
        code: None,
        http_status: None,
        message,
        retry: pl_protocol::RetryDisposition::Permanent,
    }
}

async fn cleanup_terminal_reviewer(
    ops: &ProjectReviewTaskContext<impl ProjectReviewWorkerOps>,
    job: &ProjectReviewJobSummary,
) {
    let reviewer_id = match ops.ops.find_project_review_job_reviewer(job.clone()).await {
        Ok(reviewer_id) => reviewer_id,
        Err(error) => {
            tracing::warn!(job_id = %job.id, "failed to locate terminal reviewer's owner: {error}");
            return;
        }
    };
    if let Some(reviewer_id) = reviewer_id {
        match tokio::time::timeout(
            Duration::from_secs(2 * 60),
            ops.ops.delete_agent(reviewer_id),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(job_id = %job.id, reviewer_id = %reviewer_id, "failed to clean up terminal reviewer: {error}");
            }
            Err(_) => {
                tracing::warn!(job_id = %job.id, reviewer_id = %reviewer_id, "terminal reviewer cleanup exceeded two minutes");
            }
        }
    }
}

async fn project_job_state_projection(
    ops: &ProjectReviewTaskContext<impl ProjectReviewWorkerOps>,
    job: &ProjectReviewJobSummary,
    outcome: Option<ProjectReviewOutcome>,
    summary: Option<String>,
) {
    refresh_project_review_job_projection(&ops.ops, ops.project_id, job, outcome, summary).await;
}

pub(crate) async fn refresh_project_review_job_projection(
    ops: &impl ProjectReviewWorkerOps,
    project_id: mai_protocol::ProjectId,
    changed_job: &ProjectReviewJobSummary,
    outcome: Option<ProjectReviewOutcome>,
    summary: Option<String>,
) {
    let active_job = match ops.load_active_project_review_job(project_id).await {
        Ok(active_job) => active_job,
        Err(error) => {
            tracing::warn!(project_id = %project_id, "failed to load active review job projection: {error}");
            return;
        }
    };
    let job = active_job.as_ref().unwrap_or(changed_job);
    let (auto_review_enabled, current_reviewer_agent_id) = match ops.project(project_id).await {
        Ok(project) => {
            let summary = project.summary.read().await;
            (
                summary.auto_review_enabled,
                summary.current_reviewer_agent_id,
            )
        }
        Err(_) => return,
    };
    let status = super::job::project_review_status_for_job(auto_review_enabled, Some(job));
    let reviewer_update = if job.status.is_terminal() {
        ReviewerAgentUpdate::Clear
    } else {
        match job.reviewer_agent_id {
            Some(reviewer_id) if current_reviewer_agent_id == Some(reviewer_id) => {
                ReviewerAgentUpdate::Keep
            }
            Some(reviewer_id) => ReviewerAgentUpdate::Set(reviewer_id),
            None => ReviewerAgentUpdate::Clear,
        }
    };
    let _ = ops
        .set_project_review_state(
            project_id,
            status,
            ReviewStateUpdate {
                current_reviewer_agent_id: reviewer_update,
                next_review_at: job.next_attempt_at,
                outcome,
                summary_text: summary,
                error: job.failure.as_ref().map(|failure| failure.message.clone()),
                ..Default::default()
            },
        )
        .await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration as StdDuration;

    use mai_protocol::{
        ProjectReviewDecision, ProjectReviewJobStatus, ProjectReviewSubmissionReceipt,
    };
    use pretty_assertions::assert_eq;
    use reqwest::StatusCode;

    use super::*;

    #[test]
    fn submitted_result_requires_a_persisted_github_receipt() {
        let mut job = job();

        apply_review_cycle_result(&mut job, submitted_result());

        assert_eq!(ProjectReviewJobStatus::Failed, job.status);
        assert_eq!(
            Some("missing_submission_receipt"),
            job.failure
                .as_ref()
                .and_then(|failure| failure.code.as_deref())
        );
    }

    #[test]
    fn persisted_receipt_makes_submitted_result_succeed() {
        let mut job = job();
        job.submission_receipt = Some(ProjectReviewSubmissionReceipt {
            github_review_id: 42,
            event: ProjectReviewDecision::Approve,
            head_sha: job.head_sha.clone(),
            html_url: None,
            submitted_at: Utc::now(),
        });

        apply_review_cycle_result(&mut job, submitted_result());

        assert_eq!(ProjectReviewJobStatus::Succeeded, job.status);
    }

    #[test]
    fn heartbeat_does_not_cancel_turn_after_submission_receipt_is_persisted() {
        let mut job = job();
        assert!(heartbeat_lease_loss_requires_attempt_cancellation(Some(
            &job
        )));

        job.submission_receipt = Some(ProjectReviewSubmissionReceipt {
            github_review_id: 42,
            event: ProjectReviewDecision::Approve,
            head_sha: job.head_sha.clone(),
            html_url: None,
            submitted_at: Utc::now(),
        });

        assert!(!heartbeat_lease_loss_requires_attempt_cancellation(Some(
            &job
        )));
        assert!(heartbeat_lease_loss_requires_attempt_cancellation(None));
    }

    #[test]
    fn runtime_failure_preserves_structured_provider_capacity_retry() {
        let error = RuntimeError::Model(pl_protocol::PureError::TransientModelTransport {
            message: "server is overloaded".to_string(),
            retry_after_ms: Some(12_000),
            code: Some("server_is_overloaded".to_string()),
            http_status: Some(503),
        });

        assert_eq!(
            ProjectReviewFailure {
                category: ProjectReviewFailureCategory::Provider,
                code: Some("server_is_overloaded".to_string()),
                http_status: Some(503),
                message: "model error: transient model transport error: server is overloaded"
                    .to_string(),
                retry: pl_protocol::RetryDisposition::Retryable {
                    retry_after_ms: Some(12_000),
                },
            },
            runtime_failure(&error)
        );
    }

    #[test]
    fn runtime_failure_preserves_github_retry_after() {
        let error = RuntimeError::GithubUnavailable {
            operation: "submit review".to_string(),
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "temporarily unavailable".to_string(),
            retry_after: Some(StdDuration::from_secs(9)),
        };

        let failure = runtime_failure(&error);

        assert_eq!(ProjectReviewFailureCategory::Github, failure.category);
        assert_eq!(Some(503), failure.http_status);
        assert_eq!(
            pl_protocol::RetryDisposition::Retryable {
                retry_after_ms: Some(9_000),
            },
            failure.retry
        );
    }

    #[test]
    fn runtime_failure_keeps_invalid_workspace_input_permanent() {
        let failure = runtime_failure(&RuntimeError::InvalidInput(
            "workspace source must be resolved before container startup".to_string(),
        ));

        assert_eq!(ProjectReviewFailureCategory::Validation, failure.category);
        assert_eq!(pl_protocol::RetryDisposition::Permanent, failure.retry);
    }

    fn job() -> ProjectReviewJobSummary {
        super::super::job::new_project_review_job(super::super::job::NewProjectReviewJob {
            project_id: uuid::Uuid::new_v4(),
            pr: 42,
            head_sha: "head".to_string(),
            source: mai_protocol::ProjectReviewJobSource::Manual,
            delivery_id: None,
            reason: "test".to_string(),
        })
    }

    fn submitted_result() -> super::super::ProjectReviewCycleResult {
        super::super::ProjectReviewCycleResult {
            outcome: ProjectReviewOutcome::ReviewSubmitted,
            review_event: Some(ProjectReviewDecision::Approve),
            pr: Some(42),
            summary: Some("submitted".to_string()),
            error: None,
            failure: None,
        }
    }
}
