use chrono::{DateTime, Utc};
use mai_protocol::{
    ProjectReviewJobStatus, ProjectReviewJobSummary, ProjectReviewRunDetail,
    ProjectReviewRunSummary,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use super::storage::{load_job, open_review_job_connection, project_review_run_summary_record};
use crate::{MaiStore, Result, StoreError, u64_to_i64};

impl MaiStore {
    pub async fn load_expired_unfinished_active_project_review_attempts(
        &self,
        recovered_at: DateTime<Utc>,
    ) -> Result<Vec<(ProjectReviewJobSummary, ProjectReviewRunSummary)>> {
        self.load_expired_unfinished_project_review_attempts(
            "AND job.active_run_id = run.id AND job.status IN \
             ('preparing','running','submission_pending','reconciling')",
            recovered_at,
        )
        .await
    }

    pub async fn load_expired_unfinished_terminal_project_review_attempts(
        &self,
        recovered_at: DateTime<Utc>,
    ) -> Result<Vec<(ProjectReviewJobSummary, ProjectReviewRunSummary)>> {
        self.load_expired_unfinished_project_review_attempts(
            "AND job.active_run_id = run.id AND job.status IN \
             ('succeeded','failed','cancelled','superseded','skipped')",
            recovered_at,
        )
        .await
    }

    async fn load_expired_unfinished_project_review_attempts(
        &self,
        scope_clause: &'static str,
        recovered_at: DateTime<Utc>,
    ) -> Result<Vec<(ProjectReviewJobSummary, ProjectReviewRunSummary)>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            let runs = {
                let mut statement = connection.prepare(&format!(
                    "SELECT run.id, run.project_id, run.job_id, run.attempt_index, \
                     run.reviewer_agent_id, run.turn_id, run.started_at, run.finished_at, \
                     run.status, run.outcome, run.review_event, run.pr, run.summary, run.error, \
                     run.failure_json, run.input_tokens, run.cached_input_tokens, \
                     run.output_tokens, run.reasoning_output_tokens, run.total_tokens, \
                     run.history_status, run.history_archive_id, run.history_archived_at \
                     FROM project_review_runs run JOIN project_review_jobs job ON job.id = run.job_id \
                     WHERE run.finished_at IS NULL {scope_clause} AND ( \
                     job.lease_expires_at IS NULL OR job.lease_expires_at <= ?1) \
                     ORDER BY run.started_at ASC, run.id ASC"
                ))?;
                statement
                    .query_map(
                        [recovered_at.to_rfc3339()],
                        project_review_run_summary_record,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut attempts = Vec::with_capacity(runs.len());
            for run in runs {
                let run = run.into_summary()?;
                let job_id = run.job_id.ok_or_else(|| {
                    StoreError::DataIntegrity(format!(
                        "review run {} has no owning job",
                        run.id
                    ))
                })?;
                let job = load_job(&connection, job_id)?
                    .ok_or_else(|| {
                        StoreError::DataIntegrity(format!(
                            "review run {} references missing job {job_id}",
                            run.id
                        ))
                    })?
                    .into_summary()?;
                attempts.push((job, run));
            }
            Ok(attempts)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!(
                "unfinished review attempt lookup failed: {error}"
            ))
        })?
    }

    pub async fn load_project_review_job_attempts(
        &self,
        job_id: Uuid,
        expected_attempt_count: u32,
    ) -> Result<Vec<ProjectReviewRunSummary>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            let mut statement = connection.prepare(
                "SELECT id, project_id, job_id, attempt_index, reviewer_agent_id, turn_id, \
                 started_at, finished_at, status, outcome, review_event, pr, summary, error, \
                 failure_json, input_tokens, cached_input_tokens, output_tokens, \
                 reasoning_output_tokens, total_tokens, history_status, history_archive_id, \
                 history_archived_at \
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
            let actual_attempt_count = attempts.len();
            if actual_attempt_count != expected_attempt_count as usize {
                return Err(StoreError::DataIntegrity(format!(
                    "review job {job_id} declares {expected_attempt_count} attempts but stores {actual_attempt_count} runs"
                )));
            }
            Ok(attempts)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("review job attempts task failed: {error}"))
        })?
    }

    pub async fn release_expired_archived_terminal_project_review_ownership(
        &self,
        recovered_at: DateTime<Utc>,
    ) -> Result<usize> {
        let path = self.path.clone();
        crate::sqlite_busy::retry_sqlite_busy(|| {
            let path = path.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let connection = open_review_job_connection(&path)?;
                    Ok(connection.execute(
                        "UPDATE project_review_jobs AS job
                         SET active_run_id = NULL, lease_owner = NULL, lease_expires_at = NULL
                         WHERE job.status IN
                         ('succeeded','failed','cancelled','superseded','skipped')
                         AND job.active_run_id IS NOT NULL
                         AND (job.lease_expires_at IS NULL OR job.lease_expires_at <= ?1)
                         AND EXISTS (
                             SELECT 1 FROM project_review_runs run
                             WHERE run.id = job.active_run_id AND run.job_id = job.id
                             AND run.finished_at IS NOT NULL
                         )",
                        [recovered_at.to_rfc3339()],
                    )?)
                })
                .await
                .map_err(|error| {
                    StoreError::InvalidConfig(format!(
                        "archived review ownership release task failed: {error}"
                    ))
                })?
            }
        })
        .await
    }

    pub async fn begin_claimed_project_review_attempt(
        &self,
        job_id: Uuid,
        owner: String,
        run_id: Uuid,
        started_at: DateTime<Utc>,
    ) -> Result<ProjectReviewJobSummary> {
        let path = self.path.clone();
        crate::sqlite_busy::retry_sqlite_busy(|| {
            let path = path.clone();
            let owner = owner.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let mut connection = open_review_job_connection(&path)?;
                    let transaction =
                        connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let mut job = load_job(&transaction, job_id)?
                        .ok_or_else(|| {
                            StoreError::InvalidConfig("review job not found".to_string())
                        })?
                        .into_summary()?;
                    if job.status != ProjectReviewJobStatus::Preparing
                        || job.lease_owner.as_deref() != Some(owner.as_str())
                        || job.active_run_id.is_some()
                    {
                        return Err(StoreError::InvalidConfig(format!(
                            "review job {job_id} is not an unstarted attempt owned by {owner}"
                        )));
                    }
                    job.attempt_count = job.attempt_count.checked_add(1).ok_or_else(|| {
                        StoreError::DataIntegrity(format!(
                            "review job {job_id} attempt count overflow"
                        ))
                    })?;
                    job.active_run_id = Some(run_id);
                    job.failure = None;
                    job.updated_at = started_at;
                    super::upsert_job(&transaction, &job)?;
                    transaction.execute(
                        "INSERT INTO project_review_runs (
                    id, project_id, job_id, attempt_index, reviewer_agent_id, turn_id,
                    started_at, finished_at, status, outcome, review_event, pr, summary, error,
                    failure_json, input_tokens, cached_input_tokens, output_tokens,
                    reasoning_output_tokens, total_tokens, history_json, history_status,
                    history_archive_id, history_archived_at
                 ) VALUES (?1,?2,?3,?4,?5,NULL,?6,NULL,'syncing',NULL,NULL,?7,NULL,NULL,NULL,0,0,0,0,0,NULL,'available',NULL,NULL)",
                params![
                    run_id.to_string(),
                    job.project_id.to_string(),
                    job.id.to_string(),
                    i64::from(job.attempt_count),
                    job.reviewer_agent_id.map(|id| id.to_string()),
                    started_at.to_rfc3339(),
                    u64_to_i64(job.pr),
                ],
                    )?;
                    transaction.commit()?;
                    Ok(job)
                })
                .await
                .map_err(|error| {
                    StoreError::InvalidConfig(format!(
                        "review attempt start task failed: {error}"
                    ))
                })?
            }
        })
        .await
    }

    pub async fn update_active_project_review_run_turn(
        &self,
        project_id: Uuid,
        run_id: Uuid,
        reviewer_agent_id: Uuid,
        turn_id: String,
    ) -> Result<()> {
        let path = self.path.clone();
        crate::sqlite_busy::retry_sqlite_busy(|| {
            let path = path.clone();
            let turn_id = turn_id.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let connection = open_review_job_connection(&path)?;
                    let changed = connection.execute(
                        "UPDATE project_review_runs AS run SET reviewer_agent_id = ?1,
                         turn_id = ?2, status = 'running'
                         WHERE run.id = ?3 AND run.project_id = ?4 AND run.finished_at IS NULL
                         AND EXISTS (
                             SELECT 1 FROM project_review_jobs job
                             WHERE job.id = run.job_id AND job.active_run_id = run.id
                             AND job.status IN
                             ('preparing','running','submission_pending','reconciling')
                         )",
                        params![
                            reviewer_agent_id.to_string(),
                            turn_id,
                            run_id.to_string(),
                            project_id.to_string(),
                        ],
                    )?;
                    if changed != 1 {
                        return Err(StoreError::DataIntegrity(format!(
                            "review run {run_id} is no longer the active unfinished attempt"
                        )));
                    }
                    Ok(())
                })
                .await
                .map_err(|error| {
                    StoreError::InvalidConfig(format!(
                        "review run turn update task failed: {error}"
                    ))
                })?
            }
        })
        .await
    }

    pub async fn finish_project_review_run(&self, run: &ProjectReviewRunDetail) -> Result<()> {
        let path = self.path.clone();
        crate::sqlite_busy::retry_sqlite_busy(|| {
            let path = path.clone();
            let run = run.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let mut connection = open_review_job_connection(&path)?;
                    let transaction =
                        connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let finished_at = run.summary.finished_at.ok_or_else(|| {
                        StoreError::DataIntegrity(format!(
                            "review run {} finalization has no finished_at",
                            run.summary.id
                        ))
                    })?;
                    let stored = transaction
                        .query_row(
                            "SELECT project_id, job_id, finished_at FROM project_review_runs WHERE id = ?1",
                            params![run.summary.id.to_string()],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, Option<String>>(1)?,
                                    row.get::<_, Option<String>>(2)?,
                                ))
                            },
                        )
                        .optional()?
                        .ok_or_else(|| {
                            StoreError::DataIntegrity(format!(
                                "review run {} does not exist before finalization",
                                run.summary.id
                            ))
                        })?;
                    if stored.0 != run.summary.project_id.to_string()
                        || stored.1 != run.summary.job_id.map(|job_id| job_id.to_string())
                    {
                        return Err(StoreError::DataIntegrity(format!(
                            "review run {} ownership changed before finalization",
                            run.summary.id
                        )));
                    }
                    if stored.2.is_some() {
                        transaction.commit()?;
                        return Ok(());
                    }
                    let terminal_job = if let Some(job_id) = run.summary.job_id {
                        let job = load_job(&transaction, job_id)?
                            .ok_or_else(|| {
                                StoreError::DataIntegrity(format!(
                                    "review run {} references missing job {job_id}",
                                    run.summary.id
                                ))
                            })?
                            .into_summary()?;
                        if job.active_run_id != Some(run.summary.id) {
                            return Err(StoreError::DataIntegrity(format!(
                                "review job {job_id} owns run {:?}, not {}",
                                job.active_run_id, run.summary.id
                            )));
                        }
                        job.status.is_terminal().then_some(job_id)
                    } else {
                        None
                    };
                    let changed = transaction.execute(
                        "UPDATE project_review_runs SET reviewer_agent_id = ?1, turn_id = ?2,
                            finished_at = ?3, status = ?4, outcome = ?5, review_event = ?6,
                            pr = ?7, summary = ?8, error = ?9, failure_json = ?10,
                            input_tokens = ?11, cached_input_tokens = ?12, output_tokens = ?13,
                            reasoning_output_tokens = ?14, total_tokens = ?15,
                            history_json = COALESCE(?16, history_json)
                         WHERE id = ?17 AND finished_at IS NULL",
                        params![
                            run.summary.reviewer_agent_id.map(|id| id.to_string()),
                            run.summary.turn_id,
                            finished_at.to_rfc3339(),
                            run.summary.status.to_string(),
                            run.summary.outcome.map(|outcome| outcome.to_string()),
                            run.summary.review_event.map(|event| event.to_string()),
                            run.summary.pr.map(u64_to_i64),
                            run.summary.summary,
                            run.summary.error,
                            run.summary.failure.map(|failure| serde_json::to_string(&failure)).transpose()?,
                            u64_to_i64(run.summary.token_usage.prompt_tokens),
                            u64_to_i64(run.summary.token_usage.cached_prompt_tokens),
                            u64_to_i64(run.summary.token_usage.completion_tokens),
                            u64_to_i64(run.summary.token_usage.reasoning_tokens),
                            u64_to_i64(run.summary.token_usage.total_tokens),
                            run.history.map(|history| serde_json::to_string(&history)).transpose()?,
                            run.summary.id.to_string(),
                        ],
                    )?;
                    if changed != 1 {
                        return Err(StoreError::DataIntegrity(format!(
                            "review run {} changed concurrently during finalization",
                            run.summary.id
                        )));
                    }
                    if let Some(job_id) = terminal_job {
                        transaction.execute(
                            "UPDATE project_review_jobs SET active_run_id = NULL, \
                             lease_owner = NULL, lease_expires_at = NULL \
                             WHERE id = ?1",
                            params![job_id.to_string()],
                        )?;
                    }
                    transaction.commit()?;
                    Ok(())
                })
                .await
                .map_err(|error| {
                    StoreError::InvalidConfig(format!(
                        "review run finalization task failed: {error}"
                    ))
                })?
            }
        })
        .await
    }
}
