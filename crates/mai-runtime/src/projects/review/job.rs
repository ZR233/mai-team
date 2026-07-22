use chrono::{DateTime, TimeDelta, Utc};
use mai_protocol::{
    ProjectId, ProjectReviewFailure, ProjectReviewJobSource, ProjectReviewJobStatus,
    ProjectReviewJobSummary, ProjectReviewStatus, now,
};
use uuid::Uuid;

pub(crate) const MAX_REVIEW_JOB_ATTEMPTS: u32 = 5;
pub(crate) const REVIEW_JOB_RETRY_WINDOW_MINUTES: i64 = 30;
pub(crate) const REVIEW_JOB_LEASE_SECONDS: i64 = 60;
pub(crate) const REVIEW_JOB_HEARTBEAT_SECONDS: u64 = 15;
const RETRY_DELAYS_SECONDS: [i64; 4] = [5, 30, 120, 300];

pub(crate) struct NewProjectReviewJob {
    pub(crate) project_id: ProjectId,
    pub(crate) pr: u64,
    pub(crate) head_sha: String,
    pub(crate) source: ProjectReviewJobSource,
    pub(crate) delivery_id: Option<String>,
    pub(crate) reason: String,
}

pub(crate) fn new_project_review_job(input: NewProjectReviewJob) -> ProjectReviewJobSummary {
    let created_at = now();
    ProjectReviewJobSummary {
        id: Uuid::new_v4(),
        project_id: input.project_id,
        pr: input.pr,
        head_sha: input.head_sha,
        source: input.source,
        delivery_id: input.delivery_id,
        reason: input.reason,
        status: ProjectReviewJobStatus::Queued,
        attempt_count: 0,
        max_attempts: MAX_REVIEW_JOB_ATTEMPTS,
        first_retryable_failure_at: None,
        next_attempt_at: Some(created_at),
        reviewer_agent_id: None,
        active_run_id: None,
        lease_owner: None,
        lease_expires_at: None,
        failure: None,
        submission_intent: None,
        submission_receipt: None,
        created_at,
        updated_at: created_at,
        finished_at: None,
    }
}

pub(crate) fn schedule_retry(
    job: &mut ProjectReviewJobSummary,
    failure: ProjectReviewFailure,
    current_time: DateTime<Utc>,
) -> bool {
    let first_failure = job.first_retryable_failure_at.unwrap_or(current_time);
    let deadline = first_failure + TimeDelta::minutes(REVIEW_JOB_RETRY_WINDOW_MINUTES);
    if job.attempt_count >= job.max_attempts || current_time >= deadline {
        fail_job(job, failure, current_time);
        return false;
    }
    let delay_index = job.attempt_count.saturating_sub(1) as usize;
    let local_delay = RETRY_DELAYS_SECONDS
        .get(delay_index)
        .copied()
        .unwrap_or(*RETRY_DELAYS_SECONDS.last().expect("retry delays"));
    let jittered = jitter_seconds(local_delay, job.id, job.attempt_count);
    let provider_delay = failure
        .retry
        .retry_after_ms()
        .map(|milliseconds| milliseconds.div_ceil(1_000) as i64)
        .unwrap_or_default();
    let next_attempt_at = current_time + TimeDelta::seconds(jittered.max(provider_delay));
    if next_attempt_at > deadline {
        fail_job(job, failure, current_time);
        return false;
    }
    job.status = ProjectReviewJobStatus::RetryWaiting;
    job.first_retryable_failure_at = Some(first_failure);
    job.next_attempt_at = Some(next_attempt_at);
    job.active_run_id = None;
    job.lease_owner = None;
    job.lease_expires_at = None;
    job.failure = Some(failure);
    job.updated_at = current_time;
    true
}

pub(crate) fn fail_job(
    job: &mut ProjectReviewJobSummary,
    failure: ProjectReviewFailure,
    current_time: DateTime<Utc>,
) {
    job.status = ProjectReviewJobStatus::Failed;
    job.failure = Some(failure);
    finish_job(job, current_time);
}

pub(crate) fn succeed_job(job: &mut ProjectReviewJobSummary, current_time: DateTime<Utc>) {
    job.status = ProjectReviewJobStatus::Succeeded;
    job.failure = None;
    finish_job(job, current_time);
}

pub(crate) fn supersede_job(job: &mut ProjectReviewJobSummary, current_time: DateTime<Utc>) {
    job.status = ProjectReviewJobStatus::Superseded;
    finish_job(job, current_time);
}

pub(crate) fn project_review_status_for_job(
    auto_review_enabled: bool,
    job: Option<&ProjectReviewJobSummary>,
) -> ProjectReviewStatus {
    let Some(job) = job else {
        return if auto_review_enabled {
            ProjectReviewStatus::Idle
        } else {
            ProjectReviewStatus::Disabled
        };
    };
    match job.status {
        ProjectReviewJobStatus::Queued => ProjectReviewStatus::Queued,
        ProjectReviewJobStatus::Preparing => ProjectReviewStatus::Preparing,
        ProjectReviewJobStatus::Running | ProjectReviewJobStatus::SubmissionPending => {
            ProjectReviewStatus::Running
        }
        ProjectReviewJobStatus::RetryWaiting => ProjectReviewStatus::RetryWaiting,
        ProjectReviewJobStatus::Reconciling => ProjectReviewStatus::Reconciling,
        ProjectReviewJobStatus::Succeeded
        | ProjectReviewJobStatus::Cancelled
        | ProjectReviewJobStatus::Superseded => ProjectReviewStatus::Idle,
        ProjectReviewJobStatus::Failed => ProjectReviewStatus::Failed,
    }
}

fn finish_job(job: &mut ProjectReviewJobSummary, current_time: DateTime<Utc>) {
    job.next_attempt_at = None;
    job.active_run_id = None;
    job.lease_owner = None;
    job.lease_expires_at = None;
    job.updated_at = current_time;
    job.finished_at = Some(current_time);
}

fn jitter_seconds(base: i64, job_id: Uuid, attempt_count: u32) -> i64 {
    let bytes = job_id.as_bytes();
    let sample = u16::from_be_bytes([
        bytes[(attempt_count as usize) % bytes.len()],
        bytes[(attempt_count as usize + 7) % bytes.len()],
    ]);
    let percent = i64::from(sample % 41) - 20;
    (base + base * percent / 100).max(1)
}

#[cfg(test)]
mod tests {
    use mai_protocol::ProjectReviewFailureCategory;
    use pl_protocol::RetryDisposition;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn retry_policy_stops_after_five_total_attempts() {
        let mut job = job();
        let started_at = now();
        job.attempt_count = 5;

        assert!(!schedule_retry(
            &mut job,
            retryable_failure(None),
            started_at
        ));
        assert_eq!(ProjectReviewJobStatus::Failed, job.status);
    }

    #[test]
    fn retry_after_wins_over_local_delay_within_window() {
        let mut job = job();
        let started_at = now();
        job.attempt_count = 1;

        assert!(schedule_retry(
            &mut job,
            retryable_failure(Some(20_000)),
            started_at
        ));
        assert_eq!(
            Some(started_at + TimeDelta::seconds(20)),
            job.next_attempt_at
        );
    }

    #[test]
    fn retry_after_cannot_escape_thirty_minute_window() {
        let mut job = job();
        let started_at = now();
        job.attempt_count = 1;

        assert!(!schedule_retry(
            &mut job,
            retryable_failure(Some(31 * 60 * 1_000)),
            started_at
        ));
        assert_eq!(ProjectReviewJobStatus::Failed, job.status);
    }

    fn job() -> ProjectReviewJobSummary {
        new_project_review_job(NewProjectReviewJob {
            project_id: Uuid::new_v4(),
            pr: 42,
            head_sha: "head".to_string(),
            source: ProjectReviewJobSource::Manual,
            delivery_id: None,
            reason: "test".to_string(),
        })
    }

    fn retryable_failure(retry_after_ms: Option<u64>) -> ProjectReviewFailure {
        ProjectReviewFailure {
            category: ProjectReviewFailureCategory::ProviderCapacity,
            code: Some("server_is_overloaded".to_string()),
            http_status: None,
            message: "overloaded".to_string(),
            retry: RetryDisposition::Retryable { retry_after_ms },
        }
    }
}
