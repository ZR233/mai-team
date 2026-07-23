use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::records::{ProjectReviewJobRecord, ProjectReviewRunRecord};
use crate::*;

const REVIEW_JOB_SQLITE_BUSY_TIMEOUT_SECS: u64 = 30;
const ACTIVE_JOB_STATUSES: [&str; 6] = [
    "queued",
    "preparing",
    "running",
    "retry_waiting",
    "submission_pending",
    "reconciling",
];

pub(super) fn load_job(
    connection: &Connection,
    job_id: Uuid,
) -> Result<Option<ProjectReviewJobRecord>> {
    Ok(connection
        .query_row(
            "SELECT id, project_id, pr, head_sha, source, delivery_id, reason, status, \
             attempt_count, max_attempts, first_retryable_failure_at, next_attempt_at, \
             reviewer_agent_id, active_run_id, lease_owner, lease_expires_at, failure_json, skip_reason, \
             submission_intent_json, submission_receipt_json, created_at, updated_at, finished_at \
             FROM project_review_jobs WHERE id = ?1",
            params![job_id.to_string()],
            project_review_job_record,
        )
        .optional()?)
}

pub(super) fn load_job_by_delivery(
    connection: &Connection,
    project_id: ProjectId,
    pr: u64,
    delivery_id: &str,
) -> Result<Option<ProjectReviewJobRecord>> {
    Ok(connection
        .query_row(
            "SELECT id, project_id, pr, head_sha, source, delivery_id, reason, status, \
             attempt_count, max_attempts, first_retryable_failure_at, next_attempt_at, \
             reviewer_agent_id, active_run_id, lease_owner, lease_expires_at, failure_json, skip_reason, \
             submission_intent_json, submission_receipt_json, created_at, updated_at, finished_at \
             FROM project_review_jobs WHERE project_id = ?1 AND pr = ?2 AND delivery_id = ?3 LIMIT 1",
            params![project_id.to_string(), u64_to_i64(pr), delivery_id],
            project_review_job_record,
        )
        .optional()?)
}

pub(super) fn load_active_job(
    connection: &Connection,
    project_id: ProjectId,
    pr: u64,
) -> Result<Option<ProjectReviewJobRecord>> {
    let placeholders = ACTIVE_JOB_STATUSES
        .map(|status| format!("'{status}'"))
        .join(",");
    let sql = format!(
        "SELECT id, project_id, pr, head_sha, source, delivery_id, reason, status, \
         attempt_count, max_attempts, first_retryable_failure_at, next_attempt_at, \
         reviewer_agent_id, active_run_id, lease_owner, lease_expires_at, failure_json, skip_reason, \
         submission_intent_json, submission_receipt_json, created_at, updated_at, finished_at \
         FROM project_review_jobs WHERE project_id = ?1 AND pr = ?2 AND status IN ({placeholders}) \
         ORDER BY created_at DESC LIMIT 1"
    );
    Ok(connection
        .query_row(
            &sql,
            params![project_id.to_string(), u64_to_i64(pr)],
            project_review_job_record,
        )
        .optional()?)
}

pub(super) fn load_first_active_job(
    connection: &Connection,
    project_id: ProjectId,
) -> Result<Option<ProjectReviewJobRecord>> {
    let placeholders = ACTIVE_JOB_STATUSES
        .map(|status| format!("'{status}'"))
        .join(",");
    let sql = format!(
        "SELECT id, project_id, pr, head_sha, source, delivery_id, reason, status, \
         attempt_count, max_attempts, first_retryable_failure_at, next_attempt_at, \
         reviewer_agent_id, active_run_id, lease_owner, lease_expires_at, failure_json, skip_reason, \
         submission_intent_json, submission_receipt_json, created_at, updated_at, finished_at \
         FROM project_review_jobs WHERE project_id = ?1 AND status IN ({placeholders}) \
         ORDER BY CASE status \
             WHEN 'reconciling' THEN 0 \
             WHEN 'submission_pending' THEN 1 \
             WHEN 'running' THEN 2 \
             WHEN 'preparing' THEN 3 \
             WHEN 'queued' THEN 4 \
             WHEN 'retry_waiting' THEN 5 \
             ELSE 6 END, created_at ASC LIMIT 1"
    );
    Ok(connection
        .query_row(
            &sql,
            params![project_id.to_string()],
            project_review_job_record,
        )
        .optional()?)
}

pub(super) fn project_review_job_record(row: &Row<'_>) -> rusqlite::Result<ProjectReviewJobRecord> {
    Ok(ProjectReviewJobRecord {
        id: row.get(0)?,
        project_id: row.get(1)?,
        pr: row.get(2)?,
        head_sha: row.get(3)?,
        source: row.get(4)?,
        delivery_id: row.get(5)?,
        reason: row.get(6)?,
        status: row.get(7)?,
        attempt_count: row.get(8)?,
        max_attempts: row.get(9)?,
        first_retryable_failure_at: row.get(10)?,
        next_attempt_at: row.get(11)?,
        reviewer_agent_id: row.get(12)?,
        active_run_id: row.get(13)?,
        lease_owner: row.get(14)?,
        lease_expires_at: row.get(15)?,
        failure_json: row.get(16)?,
        skip_reason: row.get(17)?,
        submission_intent_json: row.get(18)?,
        submission_receipt_json: row.get(19)?,
        created_at: row.get(20)?,
        updated_at: row.get(21)?,
        finished_at: row.get(22)?,
    })
}

pub(super) fn project_review_run_record(row: &Row<'_>) -> rusqlite::Result<ProjectReviewRunRecord> {
    Ok(ProjectReviewRunRecord {
        id: row.get(0)?,
        project_id: row.get(1)?,
        job_id: row.get(2)?,
        attempt_index: row.get(3)?,
        reviewer_agent_id: row.get(4)?,
        turn_id: row.get(5)?,
        started_at: row.get(6)?,
        finished_at: row.get(7)?,
        status: row.get(8)?,
        outcome: row.get(9)?,
        review_event: row.get(10)?,
        pr: row.get(11)?,
        summary: row.get(12)?,
        error: row.get(13)?,
        failure_json: row.get(14)?,
        input_tokens: row.get(15)?,
        cached_input_tokens: row.get(16)?,
        output_tokens: row.get(17)?,
        reasoning_output_tokens: row.get(18)?,
        total_tokens: row.get(19)?,
        messages_json: row.get(20)?,
        events_json: row.get(21)?,
    })
}

pub(super) fn open_review_job_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(REVIEW_JOB_SQLITE_BUSY_TIMEOUT_SECS))?;
    Ok(connection)
}
