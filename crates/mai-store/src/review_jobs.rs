use mai_protocol::{
    ProjectReviewFailure, ProjectReviewJobStatus, ProjectReviewSubmissionIntent,
    ProjectReviewSubmissionReceipt,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::cleanup_tasks::ensure_project_review_cleanup_tasks;
use crate::records::ProjectReviewJobRecord;
use crate::*;

mod aggregation;
pub(crate) mod storage;

use storage::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectReviewJobEnqueueDisposition {
    Queued,
    Deduped,
}

#[derive(Debug, Clone)]
pub struct ProjectReviewJobEnqueueResult {
    pub disposition: ProjectReviewJobEnqueueDisposition,
    pub job: ProjectReviewJobSummary,
}

#[derive(Debug, Clone)]
pub enum ProjectReviewUnmergedJobEnqueueResult {
    Merged,
    Enqueued(Box<ProjectReviewJobEnqueueResult>),
}

#[derive(Debug, Clone)]
pub enum ProjectReviewCiWatchEnqueueResult {
    Enqueued(Box<ProjectReviewJobEnqueueResult>),
    SignalChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectReviewSignalFreshness {
    Current,
    Watched,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectReviewCiPendingSkipResult {
    Skipped,
    SignalChanged,
    LostLease,
}

impl MaiStore {
    pub async fn prune_project_review_jobs_before_batch(
        &self,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
        batch_size: usize,
    ) -> Result<usize> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_review_job_connection(&path)?;
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let job_ids = {
                let mut statement = transaction.prepare(
                    "SELECT job.id FROM project_review_jobs job
                     WHERE job.status IN (
                         'succeeded','failed','cancelled','superseded','skipped'
                     )
                       AND job.finished_at IS NOT NULL
                       AND job.finished_at < ?1
                       AND (job.lease_expires_at IS NULL OR job.lease_expires_at <= ?2)
                       AND NOT EXISTS (
                           SELECT 1 FROM project_review_cleanup_tasks cleanup
                           WHERE cleanup.job_id = job.id
                             AND cleanup.status != 'succeeded'
                       )
                     ORDER BY job.finished_at ASC, job.id ASC LIMIT ?3",
                )?;
                statement
                    .query_map(
                        params![
                            cutoff.to_rfc3339(),
                            now.to_rfc3339(),
                            usize_to_i64(batch_size)
                        ],
                        |row| row.get::<_, String>(0),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for job_id in &job_ids {
                transaction.execute(
                    "DELETE FROM project_review_cleanup_tasks WHERE job_id = ?1",
                    params![job_id],
                )?;
                transaction.execute(
                    "DELETE FROM project_review_jobs WHERE id = ?1",
                    params![job_id],
                )?;
            }
            transaction.commit()?;
            Ok(job_ids.len())
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("review job retention task failed: {error}"))
        })?
    }

    pub async fn load_active_project_review_job_for_pr(
        &self,
        project_id: ProjectId,
        pr: u64,
    ) -> Result<Option<ProjectReviewJobSummary>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            load_active_job(&connection, project_id, pr)?
                .map(ProjectReviewJobRecord::into_summary)
                .transpose()
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("active PR review job lookup task failed: {error}"))
        })?
    }

    pub async fn load_active_project_review_job(
        &self,
        project_id: ProjectId,
    ) -> Result<Option<ProjectReviewJobSummary>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            load_first_active_job(&connection, project_id)?
                .map(ProjectReviewJobRecord::into_summary)
                .transpose()
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("active review job lookup task failed: {error}"))
        })?
    }

    pub async fn load_project_review_prs_for_head(
        &self,
        project_id: ProjectId,
        head_sha: String,
    ) -> Result<Vec<u64>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            let mut statement = connection.prepare(
                "SELECT DISTINCT pr FROM (
                    SELECT pr FROM project_review_jobs
                    WHERE project_id = ?1 AND head_sha = ?2
                    UNION ALL
                    SELECT pr FROM project_review_ci_watches
                    WHERE project_id = ?1 AND head_sha = ?2
                 ) ORDER BY pr ASC",
            )?;
            let rows = statement.query_map(params![project_id.to_string(), head_sha], |row| {
                row.get::<_, i64>(0)
            })?;
            let mut prs = Vec::new();
            for row in rows {
                prs.push(i64_to_u64(row?));
            }
            Ok(prs)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("review job PR-by-head lookup task failed: {error}"))
        })?
    }

    pub async fn project_has_active_review_jobs(&self, project_id: ProjectId) -> Result<bool> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            Ok(load_first_active_job(&connection, project_id)?.is_some())
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("review job lookup task failed: {error}"))
        })?
    }

    pub async fn enqueue_project_review_job(
        &self,
        candidate: ProjectReviewJobSummary,
    ) -> Result<ProjectReviewJobEnqueueResult> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || enqueue_on_path(&path, candidate))
            .await
            .map_err(|error| {
                StoreError::InvalidConfig(format!("review job enqueue task failed: {error}"))
            })?
    }

    pub async fn enqueue_unmerged_project_review_job(
        &self,
        candidate: ProjectReviewJobSummary,
    ) -> Result<ProjectReviewUnmergedJobEnqueueResult> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_review_job_connection(&path)?;
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let merged = transaction
                .query_row(
                    "SELECT 1 FROM project_merged_pull_requests \
                     WHERE project_id = ?1 AND pr = ?2",
                    params![candidate.project_id.to_string(), u64_to_i64(candidate.pr)],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .is_some();
            if merged {
                transaction.commit()?;
                return Ok(ProjectReviewUnmergedJobEnqueueResult::Merged);
            }
            let result = enqueue_in_transaction(
                &transaction,
                candidate,
                ProjectReviewSignalFreshness::Current,
            )?;
            transaction.commit()?;
            Ok(ProjectReviewUnmergedJobEnqueueResult::Enqueued(Box::new(
                result,
            )))
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("unmerged review job enqueue task failed: {error}"))
        })?
    }

    pub async fn enqueue_project_review_job_from_ci_watch(
        &self,
        watch: ProjectReviewCiWatch,
        candidate: ProjectReviewJobSummary,
    ) -> Result<ProjectReviewCiWatchEnqueueResult> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_review_job_connection(&path)?;
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let persisted_head = transaction
                .query_row(
                    "SELECT head_sha FROM project_review_ci_watches WHERE id = ?1",
                    params![format!("{}:{}", watch.project_id, watch.pr)],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if persisted_head.as_deref() != Some(watch.head_sha.as_str()) {
                transaction.commit()?;
                return Ok(ProjectReviewCiWatchEnqueueResult::SignalChanged);
            }
            let deleted = transaction.execute(
                "DELETE FROM project_review_ci_watches WHERE id = ?1 AND head_sha = ?2",
                params![format!("{}:{}", watch.project_id, watch.pr), watch.head_sha],
            )?;
            if deleted != 1 {
                transaction.commit()?;
                return Ok(ProjectReviewCiWatchEnqueueResult::SignalChanged);
            }
            let queued = enqueue_in_transaction(
                &transaction,
                candidate,
                ProjectReviewSignalFreshness::Watched,
            )?;
            transaction.commit()?;
            Ok(ProjectReviewCiWatchEnqueueResult::Enqueued(Box::new(
                queued,
            )))
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("CI watch review job enqueue task failed: {error}"))
        })?
    }

    pub async fn save_project_review_job(&self, job: ProjectReviewJobSummary) -> Result<()> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_review_job_connection(&path)?;
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            upsert_job(&transaction, &job)?;
            transaction.commit()?;
            Ok(())
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("review job save task failed: {error}"))
        })?
    }

    pub async fn save_claimed_project_review_job(
        &self,
        job: ProjectReviewJobSummary,
        owner: String,
    ) -> Result<bool> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_review_job_connection(&path)?;
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let existing = load_job(&transaction, job.id)?
                .ok_or_else(|| StoreError::InvalidConfig("review job not found".to_string()))?;
            if existing.lease_owner.as_deref() != Some(owner.as_str()) {
                transaction.commit()?;
                return Ok(false);
            }
            upsert_job(&transaction, &job)?;
            transaction.commit()?;
            Ok(true)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("claimed review job save task failed: {error}"))
        })?
    }

    pub async fn skip_claimed_project_review_job_for_ci_pending(
        &self,
        job_id: Uuid,
        owner: String,
        expected_delivery_id: Option<String>,
        updated_at: DateTime<Utc>,
        next_check_at: DateTime<Utc>,
    ) -> Result<ProjectReviewCiPendingSkipResult> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_review_job_connection(&path)?;
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let Some(existing) = load_job(&transaction, job_id)? else {
                transaction.commit()?;
                return Ok(ProjectReviewCiPendingSkipResult::LostLease);
            };
            if existing.lease_owner.as_deref() != Some(owner.as_str())
                || existing.status != ProjectReviewJobStatus::Preparing.to_string()
            {
                transaction.commit()?;
                return Ok(ProjectReviewCiPendingSkipResult::LostLease);
            }
            if existing.delivery_id != expected_delivery_id {
                transaction.commit()?;
                return Ok(ProjectReviewCiPendingSkipResult::SignalChanged);
            }
            let changed = transaction.execute(
                "UPDATE project_review_jobs SET status = 'skipped', failure_json = NULL, \
                 skip_reason = 'ci_pending', next_attempt_at = NULL, active_run_id = NULL, \
                 lease_owner = NULL, lease_expires_at = NULL, updated_at = ?1, finished_at = ?1 \
                 WHERE id = ?2 AND lease_owner = ?3 AND status = 'preparing'",
                params![updated_at.to_rfc3339(), job_id.to_string(), owner,],
            )?;
            if changed != 1 {
                transaction.commit()?;
                return Ok(ProjectReviewCiPendingSkipResult::LostLease);
            }
            transaction.execute(
                "INSERT INTO project_review_ci_watches (
                    id, project_id, pr, head_sha, delivery_id, reason,
                    next_check_at, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                    next_check_at = excluded.next_check_at,
                    updated_at = excluded.updated_at
                 WHERE project_review_ci_watches.head_sha = excluded.head_sha",
                params![
                    format!("{}:{}", existing.project_id, existing.pr),
                    existing.project_id,
                    existing.pr,
                    existing.head_sha,
                    Option::<String>::None,
                    format!("{}; preflight CI pending", existing.reason),
                    next_check_at.to_rfc3339(),
                    updated_at.to_rfc3339(),
                ],
            )?;
            ensure_project_review_cleanup_tasks(&transaction, job_id, updated_at)?;
            transaction.commit()?;
            Ok(ProjectReviewCiPendingSkipResult::Skipped)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!(
                "claimed review job CI pending skip task failed: {error}"
            ))
        })?
    }

    pub async fn load_project_review_job(
        &self,
        project_id: ProjectId,
        job_id: Uuid,
    ) -> Result<Option<ProjectReviewJobSummary>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            load_job(&connection, job_id)?
                .filter(|job| job.project_id == project_id.to_string())
                .map(ProjectReviewJobRecord::into_summary)
                .transpose()
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("review job load task failed: {error}"))
        })?
    }

    pub async fn load_project_review_job_attempts(
        &self,
        job_id: Uuid,
    ) -> Result<Vec<ProjectReviewRunSummary>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            let mut statement = connection.prepare(
                "SELECT id, project_id, job_id, attempt_index, reviewer_agent_id, turn_id, \
                 started_at, finished_at, status, outcome, review_event, pr, summary, error, \
                 failure_json, input_tokens, cached_input_tokens, output_tokens, \
                 reasoning_output_tokens, total_tokens \
                 FROM project_review_runs WHERE job_id = ?1 ORDER BY attempt_index ASC, started_at ASC",
            )?;
            let rows = statement.query_map(
                params![job_id.to_string()],
                project_review_run_summary_record,
            )?;
            let mut attempts = Vec::new();
            for row in rows {
                attempts.push(row?.into_summary()?);
            }
            Ok(attempts)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("review job attempts task failed: {error}"))
        })?
    }

    pub async fn claim_due_project_review_job(
        &self,
        project_id: ProjectId,
        owner: String,
        now: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> Result<Option<ProjectReviewJobSummary>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            claim_due_job_on_path(&path, project_id, &owner, now, lease_expires_at)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("review job claim task failed: {error}"))
        })?
    }

    pub async fn heartbeat_project_review_job(
        &self,
        job_id: Uuid,
        owner: String,
        updated_at: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> Result<bool> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            let changed = connection.execute(
                "UPDATE project_review_jobs SET updated_at = ?1, lease_expires_at = ?2 \
                 WHERE id = ?3 AND lease_owner = ?4 AND status IN \
                 ('preparing','running','submission_pending','reconciling')",
                params![
                    updated_at.to_rfc3339(),
                    lease_expires_at.to_rfc3339(),
                    job_id.to_string(),
                    owner
                ],
            )?;
            Ok(changed == 1)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("review job heartbeat task failed: {error}"))
        })?
    }

    pub async fn recover_expired_project_review_jobs(&self, now: DateTime<Utc>) -> Result<usize> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || recover_expired_jobs_on_path(&path, now))
            .await
            .map_err(|error| {
                StoreError::InvalidConfig(format!("review job recovery task failed: {error}"))
            })?
    }

    pub async fn cancel_active_project_review_jobs(
        &self,
        project_id: ProjectId,
        now: DateTime<Utc>,
    ) -> Result<usize> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_review_job_connection(&path)?;
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let job_ids = {
                let mut statement = transaction.prepare(
                    "SELECT id FROM project_review_jobs WHERE project_id = ?1 AND status IN
                     ('queued','preparing','running','retry_waiting','submission_pending','reconciling')",
                )?;
                statement
                    .query_map(params![project_id.to_string()], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let changed = transaction.execute(
                "UPDATE project_review_jobs SET status = 'cancelled', finished_at = ?1, \
                 updated_at = ?1, lease_owner = NULL, lease_expires_at = NULL \
                 WHERE project_id = ?2 AND status IN \
                 ('queued','preparing','running','retry_waiting','submission_pending','reconciling')",
                params![now.to_rfc3339(), project_id.to_string()],
            )?;
            for job_id in job_ids {
                ensure_project_review_cleanup_tasks(
                    &transaction,
                    parse_uuid(&job_id)?,
                    now,
                )?;
            }
            transaction.commit()?;
            Ok(changed)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("review job cancellation task failed: {error}"))
        })?
    }

    pub async fn record_project_review_submission_intent(
        &self,
        intent: ProjectReviewSubmissionIntent,
    ) -> Result<ProjectReviewJobSummary> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || record_submission_intent_on_path(&path, intent))
            .await
            .map_err(|error| {
                StoreError::InvalidConfig(format!("review submission intent task failed: {error}"))
            })?
    }

    pub async fn record_project_review_submission_receipt(
        &self,
        job_id: Uuid,
        receipt: ProjectReviewSubmissionReceipt,
    ) -> Result<ProjectReviewJobSummary> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            record_submission_receipt_on_path(&path, job_id, receipt)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("review submission receipt task failed: {error}"))
        })?
    }

    pub async fn record_project_review_submission_failure(
        &self,
        job_id: Uuid,
        failure: ProjectReviewFailure,
        updated_at: DateTime<Utc>,
    ) -> Result<ProjectReviewJobSummary> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_review_job_connection(&path)?;
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let existing = load_job(&transaction, job_id)?
                .ok_or_else(|| StoreError::InvalidConfig("review job not found".to_string()))?;
            if existing.submission_intent_json.is_none()
                || existing.submission_receipt_json.is_some()
            {
                return Err(StoreError::InvalidConfig(
                    "review submission failure requires an unresolved intent".to_string(),
                ));
            }
            transaction.execute(
                "UPDATE project_review_jobs SET failure_json = ?1, updated_at = ?2 WHERE id = ?3",
                params![
                    serde_json::to_string(&failure)?,
                    updated_at.to_rfc3339(),
                    job_id.to_string()
                ],
            )?;
            let job = load_job(&transaction, job_id)?
                .ok_or_else(|| StoreError::InvalidConfig("review job vanished".to_string()))?
                .into_summary()?;
            transaction.commit()?;
            Ok(job)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!(
                "review submission failure persistence task failed: {error}"
            ))
        })?
    }

    pub async fn mark_project_review_submission_reconciling(
        &self,
        job_id: Uuid,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            connection.execute(
                "UPDATE project_review_jobs SET status = 'reconciling', updated_at = ?1 \
                 WHERE id = ?2 AND submission_intent_json IS NOT NULL \
                 AND submission_receipt_json IS NULL",
                params![updated_at.to_rfc3339(), job_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("review reconciliation task failed: {error}"))
        })?
    }
}

fn enqueue_on_path(
    path: &Path,
    candidate: ProjectReviewJobSummary,
) -> Result<ProjectReviewJobEnqueueResult> {
    let mut connection = open_review_job_connection(path)?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let result = enqueue_in_transaction(
        &transaction,
        candidate,
        ProjectReviewSignalFreshness::Current,
    )?;
    transaction.commit()?;
    Ok(result)
}

fn enqueue_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    candidate: ProjectReviewJobSummary,
    freshness: ProjectReviewSignalFreshness,
) -> Result<ProjectReviewJobEnqueueResult> {
    if let Some(delivery_id) = candidate.delivery_id.as_deref()
        && let Some(existing) =
            load_job_by_delivery(transaction, candidate.project_id, candidate.pr, delivery_id)?
    {
        return Ok(ProjectReviewJobEnqueueResult {
            disposition: ProjectReviewJobEnqueueDisposition::Deduped,
            job: existing.into_summary()?,
        });
    }
    if let Some(existing) = load_active_job(transaction, candidate.project_id, candidate.pr)? {
        if existing.head_sha == candidate.head_sha {
            if freshness == ProjectReviewSignalFreshness::Watched {
                return Ok(ProjectReviewJobEnqueueResult {
                    disposition: ProjectReviewJobEnqueueDisposition::Deduped,
                    job: existing.into_summary()?,
                });
            }
            transaction.execute(
                "UPDATE project_review_jobs SET delivery_id = COALESCE(?1, delivery_id), \
                 reason = ?2, updated_at = ?3 WHERE id = ?4",
                params![
                    candidate.delivery_id,
                    candidate.reason,
                    candidate.updated_at.to_rfc3339(),
                    existing.id
                ],
            )?;
            let existing_id = parse_uuid(&existing.id)?;
            let job = load_job(transaction, existing_id)?
                .ok_or_else(|| {
                    StoreError::InvalidConfig("deduped review job vanished".to_string())
                })?
                .into_summary()?;
            return Ok(ProjectReviewJobEnqueueResult {
                disposition: ProjectReviewJobEnqueueDisposition::Deduped,
                job,
            });
        }
        transaction.execute(
            "UPDATE project_review_jobs SET status = 'superseded', finished_at = ?1, \
             updated_at = ?1 WHERE id = ?2",
            params![candidate.created_at.to_rfc3339(), existing.id],
        )?;
        ensure_project_review_cleanup_tasks(
            transaction,
            parse_uuid(&existing.id)?,
            candidate.created_at,
        )?;
    }
    upsert_job(transaction, &candidate)?;
    Ok(ProjectReviewJobEnqueueResult {
        disposition: ProjectReviewJobEnqueueDisposition::Queued,
        job: candidate,
    })
}

fn claim_due_job_on_path(
    path: &Path,
    project_id: ProjectId,
    owner: &str,
    now: DateTime<Utc>,
    lease_expires_at: DateTime<Utc>,
) -> Result<Option<ProjectReviewJobSummary>> {
    let mut connection = open_review_job_connection(path)?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let candidate_id = transaction
        .query_row(
            "SELECT id FROM project_review_jobs WHERE project_id = ?1 \
             AND status IN ('queued','retry_waiting','reconciling') \
             AND (next_attempt_at IS NULL OR next_attempt_at <= ?2) \
             AND (lease_expires_at IS NULL OR lease_expires_at <= ?2) \
             AND NOT EXISTS (SELECT 1 FROM project_review_jobs AS delayed_job \
                 WHERE delayed_job.project_id = ?1 \
                 AND delayed_job.status IN ('retry_waiting','reconciling') \
                 AND delayed_job.next_attempt_at > ?2) \
             AND NOT EXISTS (SELECT 1 FROM project_review_jobs AS active_lease \
                 WHERE active_lease.project_id = ?1 \
                 AND active_lease.lease_owner IS NOT NULL \
                 AND active_lease.lease_expires_at > ?2) \
             ORDER BY CASE status \
                 WHEN 'reconciling' THEN 0 WHEN 'retry_waiting' THEN 1 ELSE 2 END, \
                 created_at ASC LIMIT 1",
            params![project_id.to_string(), now.to_rfc3339()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(candidate_id) = candidate_id else {
        transaction.commit()?;
        return Ok(None);
    };
    let existing = load_job(&transaction, parse_uuid(&candidate_id)?)?
        .ok_or_else(|| StoreError::InvalidConfig("claimed review job vanished".to_string()))?;
    let next_status = if existing.submission_intent_json.is_some() {
        ProjectReviewJobStatus::Reconciling
    } else {
        ProjectReviewJobStatus::Preparing
    };
    let attempt_increment = i64::from(next_status == ProjectReviewJobStatus::Preparing);
    transaction.execute(
        "UPDATE project_review_jobs SET status = ?1, attempt_count = attempt_count + ?2, \
         next_attempt_at = NULL, lease_owner = ?3, lease_expires_at = ?4, updated_at = ?5 \
         WHERE id = ?6",
        params![
            next_status.to_string(),
            attempt_increment,
            owner,
            lease_expires_at.to_rfc3339(),
            now.to_rfc3339(),
            candidate_id
        ],
    )?;
    let job = load_job(&transaction, parse_uuid(&candidate_id)?)?
        .ok_or_else(|| StoreError::InvalidConfig("claimed review job vanished".to_string()))?
        .into_summary()?;
    transaction.commit()?;
    Ok(Some(job))
}

fn recover_expired_jobs_on_path(path: &Path, now: DateTime<Utc>) -> Result<usize> {
    let connection = open_review_job_connection(path)?;
    let now = now.to_rfc3339();
    let changed = connection.execute(
        "UPDATE project_review_jobs SET \
         status = CASE WHEN submission_intent_json IS NULL THEN 'retry_waiting' ELSE 'reconciling' END, \
         next_attempt_at = ?1, updated_at = ?1, lease_owner = NULL, lease_expires_at = NULL \
         WHERE status IN ('preparing','running','submission_pending','reconciling') \
         AND (lease_expires_at IS NULL OR lease_expires_at <= ?1)",
        params![now],
    )?;
    connection.execute(
        "UPDATE project_review_runs SET status = 'interrupted', finished_at = ?1 \
         WHERE finished_at IS NULL AND job_id IN (SELECT id FROM project_review_jobs \
         WHERE status IN ('retry_waiting','reconciling'))",
        params![now],
    )?;
    Ok(changed)
}

fn record_submission_intent_on_path(
    path: &Path,
    intent: ProjectReviewSubmissionIntent,
) -> Result<ProjectReviewJobSummary> {
    let mut connection = open_review_job_connection(path)?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let existing = load_job(&transaction, intent.job_id)?
        .ok_or_else(|| StoreError::InvalidConfig("review job not found".to_string()))?;
    if existing.submission_receipt_json.is_some() {
        return Err(StoreError::InvalidConfig(
            "review job already has a submission receipt".to_string(),
        ));
    }
    let persisted_intent = if let Some(existing_intent) = existing.submission_intent_json.as_deref()
    {
        let existing_intent =
            serde_json::from_str::<ProjectReviewSubmissionIntent>(existing_intent)?;
        let same_logical_submission = existing_intent.job_id == intent.job_id
            && existing_intent.head_sha == intent.head_sha
            && existing_intent.event == intent.event
            && existing_intent.body_hash == intent.body_hash
            && (existing_intent.comment_count == intent.comment_count
                || (existing_intent.comment_count > 0 && intent.comment_count == 0));
        if !same_logical_submission {
            return Err(StoreError::InvalidConfig(
                "review job already has a different unresolved submission intent".to_string(),
            ));
        }
        existing_intent
    } else {
        intent.clone()
    };
    let next_status = if existing.status == ProjectReviewJobStatus::Reconciling.to_string() {
        ProjectReviewJobStatus::Reconciling
    } else {
        ProjectReviewJobStatus::SubmissionPending
    };
    transaction.execute(
        "UPDATE project_review_jobs SET status = ?1, \
         submission_intent_json = ?2, updated_at = ?3 WHERE id = ?4",
        params![
            next_status.to_string(),
            serde_json::to_string(&persisted_intent)?,
            persisted_intent.created_at.to_rfc3339(),
            persisted_intent.job_id.to_string()
        ],
    )?;
    let job = load_job(&transaction, intent.job_id)?
        .ok_or_else(|| StoreError::InvalidConfig("review job vanished".to_string()))?
        .into_summary()?;
    transaction.commit()?;
    Ok(job)
}

fn record_submission_receipt_on_path(
    path: &Path,
    job_id: Uuid,
    receipt: ProjectReviewSubmissionReceipt,
) -> Result<ProjectReviewJobSummary> {
    let mut connection = open_review_job_connection(path)?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let existing = load_job(&transaction, job_id)?
        .ok_or_else(|| StoreError::InvalidConfig("review job not found".to_string()))?;
    if let Some(existing_receipt) = existing.submission_receipt_json.as_deref() {
        let existing_receipt =
            serde_json::from_str::<ProjectReviewSubmissionReceipt>(existing_receipt)?;
        if existing_receipt != receipt {
            return Err(StoreError::InvalidConfig(
                "review job already has a different submission receipt".to_string(),
            ));
        }
    }
    transaction.execute(
        "UPDATE project_review_jobs SET status = 'succeeded', submission_receipt_json = ?1, \
         finished_at = ?2, updated_at = ?2, next_attempt_at = NULL, active_run_id = NULL, \
         lease_owner = NULL, lease_expires_at = NULL, failure_json = NULL, skip_reason = NULL \
         WHERE id = ?3",
        params![
            serde_json::to_string(&receipt)?,
            receipt.submitted_at.to_rfc3339(),
            job_id.to_string()
        ],
    )?;
    let job = load_job(&transaction, job_id)?
        .ok_or_else(|| StoreError::InvalidConfig("review job vanished".to_string()))?
        .into_summary()?;
    ensure_project_review_cleanup_tasks(&transaction, job_id, receipt.submitted_at)?;
    transaction.commit()?;
    Ok(job)
}

fn upsert_job(connection: &Connection, job: &ProjectReviewJobSummary) -> Result<()> {
    connection.execute(
        "INSERT INTO project_review_jobs (id, project_id, pr, head_sha, source, delivery_id, \
         reason, status, attempt_count, max_attempts, first_retryable_failure_at, next_attempt_at, \
         reviewer_agent_id, active_run_id, lease_owner, lease_expires_at, failure_json, environment_warning_json, skip_reason, \
         submission_intent_json, submission_receipt_json, created_at, updated_at, finished_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24) \
         ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id, pr=excluded.pr, \
         head_sha=excluded.head_sha, source=excluded.source, delivery_id=excluded.delivery_id, \
         reason=excluded.reason, status=excluded.status, attempt_count=excluded.attempt_count, \
         max_attempts=excluded.max_attempts, first_retryable_failure_at=excluded.first_retryable_failure_at, \
         next_attempt_at=excluded.next_attempt_at, reviewer_agent_id=excluded.reviewer_agent_id, \
         active_run_id=excluded.active_run_id, lease_owner=excluded.lease_owner, \
         lease_expires_at=excluded.lease_expires_at, failure_json=excluded.failure_json, \
         environment_warning_json=excluded.environment_warning_json, \
         skip_reason=excluded.skip_reason, \
         submission_intent_json=excluded.submission_intent_json, \
         submission_receipt_json=excluded.submission_receipt_json, updated_at=excluded.updated_at, \
         finished_at=excluded.finished_at \
         WHERE project_review_jobs.status NOT IN \
           ('succeeded', 'failed', 'cancelled', 'superseded', 'skipped') \
           AND project_review_jobs.updated_at <= excluded.updated_at",
        params![
            job.id.to_string(),
            job.project_id.to_string(),
            u64_to_i64(job.pr),
            job.head_sha,
            job.source.to_string(),
            job.delivery_id,
            job.reason,
            job.status.to_string(),
            i64::from(job.attempt_count),
            i64::from(job.max_attempts),
            job.first_retryable_failure_at.map(|value| value.to_rfc3339()),
            job.next_attempt_at.map(|value| value.to_rfc3339()),
            job.reviewer_agent_id.map(|value| value.to_string()),
            job.active_run_id.map(|value| value.to_string()),
            job.lease_owner,
            job.lease_expires_at.map(|value| value.to_rfc3339()),
            job.failure.as_ref().map(serde_json::to_string).transpose()?,
            job.environment_warning
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            job.skip_reason.as_ref().map(ToString::to_string),
            job.submission_intent.as_ref().map(serde_json::to_string).transpose()?,
            job.submission_receipt.as_ref().map(serde_json::to_string).transpose()?,
            job.created_at.to_rfc3339(),
            job.updated_at.to_rfc3339(),
            job.finished_at.map(|value| value.to_rfc3339()),
        ],
    )?;
    if job.status.is_terminal() {
        ensure_project_review_cleanup_tasks(connection, job.id, job.updated_at)?;
    }
    Ok(())
}
