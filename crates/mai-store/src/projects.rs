use crate::records::*;
use crate::*;

impl ConfigStore {
    pub async fn save_project(&self, project: &ProjectSummary) -> Result<()> {
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
        let mut db = self.db.clone();
        Query::<List<ProjectReviewRunRecord>>::filter(
            ProjectReviewRunRecord::fields()
                .id()
                .eq(run.summary.id.to_string()),
        )
        .delete()
        .exec(&mut db)
        .await?;
        toasty::create!(ProjectReviewRunRecord {
            id: run.summary.id.to_string(),
            project_id: run.summary.project_id.to_string(),
            reviewer_agent_id: run.summary.reviewer_agent_id.map(|id| id.to_string()),
            turn_id: run.summary.turn_id.map(|id| id.to_string()),
            started_at: run.summary.started_at.to_rfc3339(),
            finished_at: run.summary.finished_at.map(|time| time.to_rfc3339()),
            status: run.summary.status.to_string(),
            outcome: run
                .summary
                .outcome
                .as_ref()
                .map(|outcome| outcome.to_string()),
            pr: run.summary.pr.map(u64_to_i64),
            summary: run.summary.summary.clone(),
            error: run.summary.error.clone(),
            messages_json: serde_json::to_string(&run.messages)?,
            events_json: serde_json::to_string(&run.events)?,
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn load_project_review_runs(
        &self,
        project_id: ProjectId,
        since: Option<DateTime<Utc>>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<ProjectReviewRunSummary>> {
        let mut rows = self.load_project_review_run_records(project_id).await?;
        if let Some(since) = since {
            let cutoff = since.to_rfc3339();
            rows.retain(|row| row.started_at >= cutoff);
        }
        rows.sort_by(|left, right| {
            right
                .started_at
                .cmp(&left.started_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        let limit = limit.max(1);
        rows.into_iter()
            .skip(offset)
            .take(limit)
            .map(ProjectReviewRunRecord::into_summary)
            .collect()
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

    pub async fn prune_project_review_runs_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        let mut db = self.db.clone();
        let cutoff = cutoff.to_rfc3339();
        let rows = Query::<List<ProjectReviewRunRecord>>::all()
            .exec(&mut db)
            .await?;
        let old_ids = rows
            .into_iter()
            .filter(|row| row.started_at < cutoff)
            .map(|row| row.id)
            .collect::<Vec<_>>();
        for id in &old_ids {
            Query::<List<ProjectReviewRunRecord>>::filter(
                ProjectReviewRunRecord::fields().id().eq(id.clone()),
            )
            .delete()
            .exec(&mut db)
            .await?;
        }
        Ok(old_ids.len())
    }

    async fn load_project_review_run_records(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<ProjectReviewRunRecord>> {
        let mut db = self.db.clone();
        Ok(Query::<List<ProjectReviewRunRecord>>::filter(
            ProjectReviewRunRecord::fields()
                .project_id()
                .eq(project_id.to_string()),
        )
        .exec(&mut db)
        .await?)
    }
}
