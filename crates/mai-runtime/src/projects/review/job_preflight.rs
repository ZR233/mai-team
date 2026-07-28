use chrono::Utc;
use mai_protocol::{
    ProjectReviewJobSource, ProjectReviewJobStatus, ProjectReviewJobSummary, ProjectReviewOutcome,
    ProjectReviewSkipReason,
};
use mai_store::ProjectReviewCiPendingSkipResult;

use crate::RuntimeError;

use super::job_worker::{apply_review_failure, project_job_state_projection, runtime_failure};
use super::worker::{ProjectReviewTaskContext, ProjectReviewWorkerOps};

pub(super) enum ProjectReviewPreflight {
    Continue(Box<ProjectReviewJobSummary>),
    Finished,
    Cancelled,
}

pub(super) async fn preflight_project_review_job(
    ops: &ProjectReviewTaskContext<impl ProjectReviewWorkerOps>,
    mut job: ProjectReviewJobSummary,
    owner: &str,
) -> ProjectReviewPreflight {
    if job.source == ProjectReviewJobSource::Manual {
        return ProjectReviewPreflight::Continue(Box::new(job));
    }
    loop {
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
                project_job_state_projection(ops, &job, Some(ProjectReviewOutcome::Failed), None)
                    .await;
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
        if evaluated.skip_reason == Some(ProjectReviewSkipReason::CiPending) {
            match skip_ci_pending_job(ops, &mut job, owner).await {
                CiPendingPreflight::Skipped => return ProjectReviewPreflight::Finished,
                CiPendingPreflight::Recheck(reloaded) => {
                    job = *reloaded;
                    continue;
                }
                CiPendingPreflight::Cancelled => return ProjectReviewPreflight::Cancelled,
                CiPendingPreflight::Failed => return ProjectReviewPreflight::Finished,
            }
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
        return ProjectReviewPreflight::Continue(Box::new(job));
    }
}

enum CiPendingPreflight {
    Skipped,
    Recheck(Box<ProjectReviewJobSummary>),
    Cancelled,
    Failed,
}

async fn skip_ci_pending_job(
    ops: &ProjectReviewTaskContext<impl ProjectReviewWorkerOps>,
    job: &mut ProjectReviewJobSummary,
    owner: &str,
) -> CiPendingPreflight {
    let now = Utc::now();
    match ops
        .ops
        .skip_claimed_project_review_job_for_ci_pending(
            job.id,
            owner.to_string(),
            job.delivery_id.clone(),
            now,
            now + chrono::TimeDelta::seconds(super::PROJECT_REVIEW_CI_WATCH_INTERVAL_SECS as i64),
        )
        .await
    {
        Ok(ProjectReviewCiPendingSkipResult::Skipped) => {
            super::job::skip_job(job, ProjectReviewSkipReason::CiPending, now);
            project_job_state_projection(ops, job, None, None).await;
            tracing::info!(
                job_id = %job.id,
                pr = job.pr,
                "skipped review job because CI is still running"
            );
            CiPendingPreflight::Skipped
        }
        Ok(ProjectReviewCiPendingSkipResult::SignalChanged) => {
            match ops
                .ops
                .load_project_review_job(job.project_id, job.id)
                .await
            {
                Ok(Some(reloaded))
                    if reloaded.lease_owner.as_deref() == Some(owner)
                        && reloaded.status == ProjectReviewJobStatus::Preparing =>
                {
                    tracing::info!(
                        job_id = %job.id,
                        pr = job.pr,
                        delivery_id = reloaded.delivery_id.as_deref().unwrap_or_default(),
                        "rechecking review job after a newer completed check signal"
                    );
                    CiPendingPreflight::Recheck(Box::new(reloaded))
                }
                Ok(_) => CiPendingPreflight::Cancelled,
                Err(error) => {
                    tracing::warn!(
                        job_id = %job.id,
                        pr = job.pr,
                        error = %error,
                        "failed to reload review job after completed check signal"
                    );
                    CiPendingPreflight::Failed
                }
            }
        }
        Ok(ProjectReviewCiPendingSkipResult::LostLease) => CiPendingPreflight::Cancelled,
        Err(error) => {
            tracing::warn!(
                job_id = %job.id,
                pr = job.pr,
                error = %error,
                "failed to persist CI-pending review job skip"
            );
            CiPendingPreflight::Failed
        }
    }
}
