use std::future::Future;
use std::time::Duration;

use mai_protocol::{
    AgentId, ProjectReviewJobStatus, ProjectReviewJobSummary, ProjectReviewOutcome,
    ProjectReviewRunStatus, ProjectReviewRunSummary, TurnId, now,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::ProjectReviewCycleResult;
use super::cycle::{
    ProjectReviewCycleOps, ReviewerProgress, last_turn_cancelled, parse_reviewer_final_response,
};
use super::reviewer::PreparedProjectReviewer;
use super::runs::FinishReviewRun;
use super::target::ProjectReviewRequest;
use crate::{Result, RuntimeError};

const REVIEW_CONTINUATION_PROMPT: &str = "Continue the same pull request review after a retryable interruption. Keep using the existing session note as the append-only findings ledger. Re-check the fixed PR head before submission and do not repeat completed investigation unnecessarily. Treat a GitHub review as already submitted by this logical Job only when it targets the fixed head and contains the exact `mai-review-job:<current job UUID>` marker identified by your system prompt, or when this same Job's final submission call returned an ambiguous network result that you are actively reconciling. Existing reviews without that exact marker, including reviews from another Job or another head, are context only and do not fulfill this Job. Never return `review_submitted` unless this Job submitted the review or you confirmed its exact marker and head. Complete the review and return only the required final JSON object.";
const REVIEW_PREPARING_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const REVIEW_RUNNING_WATCHDOG_POLL: Duration = Duration::from_secs(30);
const REVIEW_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2 * 60);

struct AttemptCompletion {
    status: ProjectReviewRunStatus,
    outcome: Option<ProjectReviewOutcome>,
    review_event: Option<mai_protocol::ProjectReviewDecision>,
    summary: Option<String>,
    error: Option<String>,
    failure: Option<mai_protocol::ProjectReviewFailure>,
}

/// 为持久化 review job 执行一次 continuation turn 所需的边界。
pub(crate) trait ProjectReviewJobAttemptOps: ProjectReviewCycleOps {
    fn save_claimed_project_review_job(
        &self,
        job: ProjectReviewJobSummary,
        owner: String,
    ) -> impl Future<Output = Result<bool>> + Send;

    fn resume_project_reviewer(
        &self,
        job: ProjectReviewJobSummary,
        reviewer_id: AgentId,
    ) -> impl Future<Output = Result<PreparedProjectReviewer>> + Send;

    fn cleanup_timed_out_review_preparation(
        &self,
        project_id: mai_protocol::ProjectId,
    ) -> impl Future<Output = Result<()>> + Send;

    fn refresh_project_review_job_projection(
        &self,
        job: ProjectReviewJobSummary,
    ) -> impl Future<Output = ()> + Send;
}

pub(crate) async fn run_project_review_job_attempt(
    ops: &impl ProjectReviewJobAttemptOps,
    mut job: ProjectReviewJobSummary,
    owner: String,
    cancellation_token: CancellationToken,
) -> Result<ProjectReviewCycleResult> {
    let run_id = Uuid::new_v4();
    job.active_run_id = Some(run_id);
    job.status = ProjectReviewJobStatus::Preparing;
    job.updated_at = now();
    save_claimed_job(ops, &job, &owner).await?;
    ops.save_project_review_run_status(ProjectReviewRunSummary {
        id: run_id,
        job_id: Some(job.id),
        attempt_index: job.attempt_count,
        project_id: job.project_id,
        reviewer_agent_id: job.reviewer_agent_id,
        turn_id: None,
        started_at: now(),
        finished_at: None,
        status: ProjectReviewRunStatus::Syncing,
        outcome: None,
        review_event: None,
        pr: Some(job.pr),
        summary: None,
        error: None,
        failure: None,
        token_usage: Default::default(),
    })
    .await?;

    if cancellation_token.is_cancelled() {
        finish_attempt(
            ops,
            &job,
            run_id,
            None,
            None,
            AttemptCompletion {
                status: ProjectReviewRunStatus::Interrupted,
                outcome: None,
                review_event: None,
                summary: None,
                error: Some("review interrupted".to_string()),
                failure: None,
            },
        )
        .await;
        return Err(RuntimeError::TurnCancelled);
    }

    let continuing = job.reviewer_agent_id.is_some();
    let prepare = async {
        match job.reviewer_agent_id {
            Some(reviewer_id) => ops.resume_project_reviewer(job.clone(), reviewer_id).await,
            None => {
                ops.prepare_project_reviewer(
                    job.project_id,
                    job.id,
                    ProjectReviewRequest {
                        pr: job.pr,
                        head_sha_hint: Some(job.head_sha.clone()),
                    },
                )
                .await
            }
        }
    };
    let prepared = tokio::select! {
        prepared = tokio::time::timeout(REVIEW_PREPARING_TIMEOUT, prepare) => prepared,
        _ = cancellation_token.cancelled() => {
            finish_attempt(
                ops,
                &job,
                run_id,
                job.reviewer_agent_id,
                None,
                AttemptCompletion {
                    status: ProjectReviewRunStatus::Interrupted,
                    outcome: None,
                    review_event: None,
                    summary: None,
                    error: Some("review interrupted while preparing".to_string()),
                    failure: None,
                },
            )
            .await;
            return Err(RuntimeError::TurnCancelled);
        }
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(_) => {
            if job.reviewer_agent_id.is_none() {
                let _ = tokio::time::timeout(
                    REVIEW_CLEANUP_TIMEOUT,
                    ops.cleanup_timed_out_review_preparation(job.project_id),
                )
                .await;
            }
            let result =
                retryable_timeout_result("review preparation made no progress for five minutes");
            finish_attempt(
                ops,
                &job,
                run_id,
                job.reviewer_agent_id,
                None,
                AttemptCompletion {
                    status: ProjectReviewRunStatus::RetryableFailed,
                    outcome: Some(result.outcome.clone()),
                    review_event: None,
                    summary: result.summary.clone(),
                    error: result.error.clone(),
                    failure: result.failure.clone(),
                },
            )
            .await;
            return Ok(result);
        }
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let failure = super::job_worker::runtime_failure(&error);
            let status = if failure.retry.is_retryable() {
                ProjectReviewRunStatus::RetryableFailed
            } else {
                ProjectReviewRunStatus::PermanentFailed
            };
            finish_attempt(
                ops,
                &job,
                run_id,
                job.reviewer_agent_id,
                None,
                AttemptCompletion {
                    status,
                    outcome: Some(ProjectReviewOutcome::Failed),
                    review_event: None,
                    summary: None,
                    error: Some(error.to_string()),
                    failure: Some(failure),
                },
            )
            .await;
            return Err(error);
        }
    };
    let reviewer_id = prepared.agent.id;
    job.reviewer_agent_id = Some(reviewer_id);
    job.status = ProjectReviewJobStatus::Running;
    job.updated_at = now();
    save_claimed_job(ops, &job, &owner).await?;
    ops.refresh_project_review_job_projection(job.clone()).await;

    let started_at = ops
        .load_project_review_run(job.project_id, run_id)
        .await?
        .map_or_else(now, |run| run.summary.started_at);
    ops.save_project_review_run_status(ProjectReviewRunSummary {
        id: run_id,
        job_id: Some(job.id),
        attempt_index: job.attempt_count,
        project_id: job.project_id,
        reviewer_agent_id: Some(reviewer_id),
        turn_id: None,
        started_at,
        finished_at: None,
        status: ProjectReviewRunStatus::Running,
        outcome: None,
        review_event: None,
        pr: Some(job.pr),
        summary: None,
        error: None,
        failure: None,
        token_usage: Default::default(),
    })
    .await?;

    let result = execute_turn(
        ops,
        &job,
        run_id,
        reviewer_id,
        prepared,
        continuing,
        &cancellation_token,
    )
    .await;
    let turn_id = ops
        .load_project_review_run(job.project_id, run_id)
        .await
        .ok()
        .flatten()
        .and_then(|run| run.summary.turn_id);
    match &result {
        Ok(cycle) => {
            let status = if cycle.outcome == ProjectReviewOutcome::Failed {
                if cycle
                    .failure
                    .as_ref()
                    .is_some_and(|failure| failure.retry.is_retryable())
                {
                    ProjectReviewRunStatus::RetryableFailed
                } else {
                    ProjectReviewRunStatus::PermanentFailed
                }
            } else {
                ProjectReviewRunStatus::Succeeded
            };
            finish_attempt(
                ops,
                &job,
                run_id,
                Some(reviewer_id),
                turn_id,
                AttemptCompletion {
                    status,
                    outcome: Some(cycle.outcome.clone()),
                    review_event: cycle.review_event.clone(),
                    summary: cycle.summary.clone(),
                    error: cycle.error.clone(),
                    failure: cycle.failure.clone(),
                },
            )
            .await;
        }
        Err(error) => {
            let failure = (!matches!(error, RuntimeError::TurnCancelled))
                .then(|| super::job_worker::runtime_failure(error));
            let status = match failure.as_ref() {
                None => ProjectReviewRunStatus::Interrupted,
                Some(failure) if failure.retry.is_retryable() => {
                    ProjectReviewRunStatus::RetryableFailed
                }
                Some(_) => ProjectReviewRunStatus::PermanentFailed,
            };
            finish_attempt(
                ops,
                &job,
                run_id,
                Some(reviewer_id),
                turn_id,
                AttemptCompletion {
                    status,
                    outcome: Some(ProjectReviewOutcome::Failed),
                    review_event: None,
                    summary: None,
                    error: Some(error.to_string()),
                    failure,
                },
            )
            .await;
        }
    }
    result
}

async fn execute_turn(
    ops: &impl ProjectReviewJobAttemptOps,
    job: &ProjectReviewJobSummary,
    run_id: Uuid,
    reviewer_id: AgentId,
    prepared: PreparedProjectReviewer,
    continuing: bool,
    cancellation_token: &CancellationToken,
) -> Result<ProjectReviewCycleResult> {
    let message = if continuing {
        REVIEW_CONTINUATION_PROMPT.to_string()
    } else {
        ops.project_reviewer_initial_message(
            job.project_id,
            reviewer_id,
            prepared.target.clone(),
            prepared.project_revision.clone(),
        )
        .await?
    };
    let turn_id = ops.start_reviewer_turn(reviewer_id, message).await?;
    ops.update_project_review_run_turn(job.project_id, run_id, reviewer_id, turn_id)
        .await?;
    let wait_result = match wait_reviewer_with_watchdog(
        ops,
        reviewer_id,
        turn_id,
        cancellation_token,
    )
    .await?
    {
        ReviewerWait::Completed(wait_result) => *wait_result,
        ReviewerWait::TimedOut => {
            return Ok(retryable_timeout_result(
                "reviewer made no model, tool, or process progress before the running watchdog deadline",
            ));
        }
    };
    if last_turn_cancelled(&wait_result) && cancellation_token.is_cancelled() {
        return Err(RuntimeError::TurnCancelled);
    }
    let result = match super::project_review_cycle_result_for_wait_result(&wait_result) {
        Some(result) => result,
        None => parse_reviewer_final_response(ops, reviewer_id, cancellation_token).await?,
    };
    validate_attempt_result(ops, reviewer_id, &prepared, result).await
}

enum ReviewerWait {
    Completed(Box<pl_core::AgentWaitResult>),
    TimedOut,
}

async fn wait_reviewer_with_watchdog(
    ops: &impl ProjectReviewJobAttemptOps,
    reviewer_id: AgentId,
    turn_id: TurnId,
    cancellation_token: &CancellationToken,
) -> Result<ReviewerWait> {
    let mut progress = ops.reviewer_progress(reviewer_id).await?;
    let mut last_progress = tokio::time::Instant::now();
    let wait = ops.wait_agent_until_complete_with_cancel(reviewer_id, cancellation_token);
    tokio::pin!(wait);
    loop {
        tokio::select! {
            result = &mut wait => return result.map(|wait_result| ReviewerWait::Completed(Box::new(wait_result))),
            _ = tokio::time::sleep(REVIEW_RUNNING_WATCHDOG_POLL) => {
                let current = ops.reviewer_progress(reviewer_id).await?;
                update_progress_deadline(&mut progress, current, &mut last_progress);
                if last_progress.elapsed() >= progress.inactivity_timeout {
                    ops.cancel_reviewer_turn(reviewer_id, turn_id).await?;
                    return Ok(ReviewerWait::TimedOut);
                }
            }
        }
    }
}

fn update_progress_deadline(
    progress: &mut ReviewerProgress,
    current: ReviewerProgress,
    last_progress: &mut tokio::time::Instant,
) {
    if current.revision != progress.revision {
        *last_progress = tokio::time::Instant::now();
    }
    *progress = current;
}

fn retryable_timeout_result(message: &str) -> ProjectReviewCycleResult {
    ProjectReviewCycleResult {
        outcome: ProjectReviewOutcome::Failed,
        review_event: None,
        pr: None,
        summary: None,
        error: Some(message.to_string()),
        failure: Some(mai_protocol::ProjectReviewFailure {
            category: mai_protocol::ProjectReviewFailureCategory::Timeout,
            code: Some("review_watchdog_timeout".to_string()),
            http_status: None,
            message: message.to_string(),
            retry: pl_protocol::RetryDisposition::Retryable {
                retry_after_ms: None,
            },
        }),
    }
}

async fn validate_attempt_result(
    ops: &impl ProjectReviewJobAttemptOps,
    reviewer_id: AgentId,
    prepared: &PreparedProjectReviewer,
    result: ProjectReviewCycleResult,
) -> Result<ProjectReviewCycleResult> {
    if ops.reviewer_target_is_stale(reviewer_id).await? {
        return Err(RuntimeError::InvalidInput(format!(
            "{} for PR #{}",
            super::REVIEW_TARGET_HEAD_CHANGED,
            prepared.target.pr
        )));
    }
    if result
        .pr
        .is_some_and(|reported_pr| reported_pr != prepared.target.pr)
    {
        return Err(RuntimeError::InvalidInput(format!(
            "reviewer reported PR #{} while the prepared target is PR #{}",
            result.pr.unwrap_or_default(),
            prepared.target.pr
        )));
    }
    Ok(result)
}

async fn save_claimed_job(
    ops: &impl ProjectReviewJobAttemptOps,
    job: &ProjectReviewJobSummary,
    owner: &str,
) -> Result<()> {
    if ops
        .save_claimed_project_review_job(job.clone(), owner.to_string())
        .await?
    {
        return Ok(());
    }
    Err(RuntimeError::InvalidInput(format!(
        "review job {} lease is no longer owned by {owner}",
        job.id
    )))
}

async fn finish_attempt(
    ops: &impl ProjectReviewJobAttemptOps,
    job: &ProjectReviewJobSummary,
    run_id: Uuid,
    reviewer_agent_id: Option<AgentId>,
    turn_id: Option<TurnId>,
    completion: AttemptCompletion,
) {
    let _ = ops
        .finish_project_review_run(FinishReviewRun {
            run_id,
            project_id: job.project_id,
            reviewer_agent_id,
            turn_id,
            status: completion.status,
            outcome: completion.outcome,
            review_event: completion.review_event,
            pr: Some(job.pr),
            summary_text: completion.summary,
            error: completion.error,
            failure: completion.failure,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn running_watchdog_resets_only_when_runtime_revision_advances() {
        let mut progress = ReviewerProgress {
            revision: 7,
            inactivity_timeout: Duration::from_secs(600),
        };
        let mut last_progress = tokio::time::Instant::now() - Duration::from_secs(90);

        update_progress_deadline(
            &mut progress,
            ReviewerProgress {
                revision: 7,
                inactivity_timeout: Duration::from_secs(660),
            },
            &mut last_progress,
        );
        assert!(last_progress.elapsed() >= Duration::from_secs(89));
        assert_eq!(Duration::from_secs(660), progress.inactivity_timeout);

        update_progress_deadline(
            &mut progress,
            ReviewerProgress {
                revision: 8,
                inactivity_timeout: Duration::from_secs(600),
            },
            &mut last_progress,
        );
        assert!(last_progress.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn watchdog_timeout_is_a_structured_retryable_failure() {
        let result = retryable_timeout_result("stalled");

        assert_eq!(ProjectReviewOutcome::Failed, result.outcome);
        assert_eq!(
            Some(mai_protocol::ProjectReviewFailureCategory::Timeout),
            result
                .failure
                .as_ref()
                .map(|failure| failure.category.clone())
        );
        assert!(
            result
                .failure
                .as_ref()
                .is_some_and(|failure| failure.retry.is_retryable())
        );
    }

    #[test]
    fn continuation_requires_the_current_job_marker_before_claiming_submission() {
        assert!(
            REVIEW_CONTINUATION_PROMPT.contains("exact `mai-review-job:<current job UUID>` marker")
        );
        assert!(REVIEW_CONTINUATION_PROMPT.contains("Existing reviews without that exact marker"));
        assert!(
            REVIEW_CONTINUATION_PROMPT
                .contains("Never return `review_submitted` unless this Job submitted")
        );
    }
}
