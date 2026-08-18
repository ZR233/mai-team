use crate::records::*;
use crate::review_jobs::storage::{open_review_job_connection, project_review_run_summary_record};
use crate::*;
use rusqlite::params;

impl MaiStore {
    pub async fn save_project(&self, project: &ProjectSummary) -> Result<()> {
        crate::sqlite_busy::retry_sqlite_busy(|| async { self.save_project_once(project).await })
            .await
    }

    async fn save_project_once(&self, project: &ProjectSummary) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<ProjectRecordRow>>::filter(
            ProjectRecordRow::fields().id().eq(project.id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        toasty::create!(ProjectRecordRow {
            id: project.id.to_string(),
            name: project.name.clone(),
            status: project.status.to_string(),
            owner: project.owner.clone(),
            repo: project.repo.clone(),
            repository_full_name: project.repository_full_name.clone(),
            git_account_id: project.git_account_id.clone(),
            repository_id: u64_to_i64(project.repository_id),
            installation_id: u64_to_i64(project.installation_id),
            installation_account: project.installation_account.clone(),
            branch: project.branch.clone(),
            docker_image: project.docker_image.clone(),
            clone_status: project.clone_status.to_string(),
            maintainer_agent_id: project.maintainer_agent_id.to_string(),
            created_at: project.created_at.to_rfc3339(),
            updated_at: project.updated_at.to_rfc3339(),
            last_error: project.last_error.clone(),
            auto_review_enabled: project.auto_review_enabled,
            reviewer_extra_prompt: project.reviewer_extra_prompt.clone(),
            review_status: project.review_status.to_string(),
            current_reviewer_agent_id: project.current_reviewer_agent_id.map(|id| id.to_string()),
            last_review_started_at: project.last_review_started_at.map(|time| time.to_rfc3339()),
            last_review_finished_at: project
                .last_review_finished_at
                .map(|time| time.to_rfc3339()),
            next_review_at: project.next_review_at.map(|time| time.to_rfc3339()),
            last_review_outcome: project.last_review_outcome.as_ref().map(|o| o.to_string()),
            review_last_error: project.review_last_error.clone(),
        })
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_project(&self, project_id: ProjectId) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<ProjectRecordRow>>::filter(
            ProjectRecordRow::fields().id().eq(project_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        Query::<List<ProjectReviewRunRecord>>::filter(
            ProjectReviewRunRecord::fields()
                .project_id()
                .eq(project_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        Query::<List<ProjectReviewJobRecord>>::filter(
            ProjectReviewJobRecord::fields()
                .project_id()
                .eq(project_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        Query::<List<ProjectReviewCiWatchRecord>>::filter(
            ProjectReviewCiWatchRecord::fields()
                .project_id()
                .eq(project_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn load_projects(&self) -> Result<Vec<ProjectSummary>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<ProjectRecordRow>>::all().exec(&mut db).await?;
        rows.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        rows.into_iter()
            .map(ProjectRecordRow::into_summary)
            .collect()
    }

    pub async fn save_project_review_run(&self, run: &ProjectReviewRunDetail) -> Result<()> {
        crate::sqlite_busy::retry_sqlite_busy(|| async {
            self.save_project_review_run_once(run).await
        })
        .await
    }

    async fn save_project_review_run_once(&self, run: &ProjectReviewRunDetail) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<ProjectReviewRunRecord>>::filter(
            ProjectReviewRunRecord::fields()
                .id()
                .eq(run.summary.id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        toasty::create!(ProjectReviewRunRecord {
            id: run.summary.id.to_string(),
            project_id: run.summary.project_id.to_string(),
            job_id: run.summary.job_id.map(|id| id.to_string()),
            attempt_index: i64::from(run.summary.attempt_index),
            reviewer_agent_id: run.summary.reviewer_agent_id.map(|id| id.to_string()),
            turn_id: run.summary.turn_id.clone(),
            started_at: run.summary.started_at.to_rfc3339(),
            finished_at: run.summary.finished_at.map(|time| time.to_rfc3339()),
            status: run.summary.status.to_string(),
            outcome: run
                .summary
                .outcome
                .as_ref()
                .map(|outcome| outcome.to_string()),
            review_event: run
                .summary
                .review_event
                .as_ref()
                .map(|event| event.to_string()),
            pr: run.summary.pr.map(u64_to_i64),
            summary: run.summary.summary.clone(),
            error: run.summary.error.clone(),
            failure_json: run
                .summary
                .failure
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            input_tokens: u64_to_i64(run.summary.token_usage.input_tokens),
            cached_input_tokens: u64_to_i64(run.summary.token_usage.cached_input_tokens),
            output_tokens: u64_to_i64(run.summary.token_usage.output_tokens),
            reasoning_output_tokens: u64_to_i64(run.summary.token_usage.reasoning_output_tokens),
            total_tokens: u64_to_i64(run.summary.token_usage.total_tokens),
            history_json: run
                .history
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        })
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn load_project_review_runs(
        &self,
        project_id: ProjectId,
        since: Option<DateTime<Utc>>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<ProjectReviewRunSummary>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            let mut runs = Vec::new();
            if let Some(since) = since {
                let mut statement = connection.prepare(
                    "SELECT id, project_id, job_id, attempt_index, reviewer_agent_id, turn_id, \
                     started_at, finished_at, status, outcome, review_event, pr, summary, error, \
                     failure_json, input_tokens, cached_input_tokens, output_tokens, \
                     reasoning_output_tokens, total_tokens \
                     FROM project_review_runs WHERE project_id = ?1 AND started_at >= ?2 \
                     ORDER BY started_at DESC, id DESC LIMIT ?3 OFFSET ?4",
                )?;
                let rows = statement.query_map(
                    params![
                        project_id.to_string(),
                        since.to_rfc3339(),
                        usize_to_i64(limit.max(1)),
                        usize_to_i64(offset)
                    ],
                    project_review_run_summary_record,
                )?;
                for row in rows {
                    runs.push(row?.into_summary()?);
                }
            } else {
                let mut statement = connection.prepare(
                    "SELECT id, project_id, job_id, attempt_index, reviewer_agent_id, turn_id, \
                     started_at, finished_at, status, outcome, review_event, pr, summary, error, \
                     failure_json, input_tokens, cached_input_tokens, output_tokens, \
                     reasoning_output_tokens, total_tokens \
                     FROM project_review_runs WHERE project_id = ?1 \
                     ORDER BY started_at DESC, id DESC LIMIT ?2 OFFSET ?3",
                )?;
                let rows = statement.query_map(
                    params![
                        project_id.to_string(),
                        usize_to_i64(limit.max(1)),
                        usize_to_i64(offset)
                    ],
                    project_review_run_summary_record,
                )?;
                for row in rows {
                    runs.push(row?.into_summary()?);
                }
            }
            Ok(runs)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("review run list task failed: {error}"))
        })?
    }

    pub async fn load_project_review_run(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
    ) -> Result<Option<ProjectReviewRunDetail>> {
        let mut db = self.db.clone();
        let row = Query::<List<ProjectReviewRunRecord>>::filter(
            ProjectReviewRunRecord::fields().id().eq(run_id.to_string()),
        )
        .first()
        .exec(&mut db)
        .await?;
        row.filter(|row| row.project_id == project_id.to_string())
            .map(ProjectReviewRunRecord::into_detail)
            .transpose()
    }

    pub async fn prune_orphan_project_review_runs_before(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<usize> {
        self.prune_orphan_project_review_runs_before_batch(cutoff, 500)
            .await
    }

    pub async fn prune_orphan_project_review_runs_before_batch(
        &self,
        cutoff: DateTime<Utc>,
        batch_size: usize,
    ) -> Result<usize> {
        let path = self.path.clone();
        crate::sqlite_busy::retry_sqlite_busy(|| {
            let path = path.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let mut connection = rusqlite::Connection::open(path)?;
                    connection.busy_timeout(std::time::Duration::from_secs(30))?;
                    let transaction = connection.transaction()?;
                    let removed = transaction.execute(
                        "DELETE FROM project_review_runs WHERE id IN (
                    SELECT run.id FROM project_review_runs run
                    WHERE (
                        run.job_id IS NULL
                        OR NOT EXISTS (
                            SELECT 1 FROM project_review_jobs job WHERE job.id = run.job_id
                        )
                    )
                      AND run.finished_at IS NOT NULL
                      AND run.finished_at < ?1
                    ORDER BY run.finished_at ASC, run.id ASC LIMIT ?2
                 )",
                        rusqlite::params![cutoff.to_rfc3339(), usize_to_i64(batch_size)],
                    )?;
                    transaction.commit()?;
                    Ok(removed)
                })
                .await
                .map_err(|error| {
                    StoreError::InvalidConfig(format!(
                        "orphan review run retention task failed: {error}"
                    ))
                })?
            }
        })
        .await
    }
}
