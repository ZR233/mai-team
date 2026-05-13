use crate::records::*;
use crate::*;

impl ConfigStore {
    pub async fn save_task(&self, task: &TaskSummary, plan: &TaskPlan) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<TaskRecordRow>>::filter(TaskRecordRow::fields().id().eq(task.id.to_string()))
            .delete()
            .exec(&mut tx)
            .await?;
        toasty::create!(TaskRecordRow {
            id: task.id.to_string(),
            title: task.title.clone(),
            status: task.status.to_string(),
            planner_agent_id: task.planner_agent_id.to_string(),
            current_agent_id: task.current_agent_id.map(|id| id.to_string()),
            created_at: task.created_at.to_rfc3339(),
            updated_at: task.updated_at.to_rfc3339(),
            last_error: task.last_error.clone(),
            final_report: task.final_report.clone(),
            plan_status: plan.status.to_string(),
            plan_title: plan.title.clone(),
            plan_markdown: plan.markdown.clone(),
            plan_version: u64_to_i64(plan.version),
            plan_saved_by_agent_id: plan.saved_by_agent_id.map(|id| id.to_string()),
            plan_saved_at: plan.saved_at.map(|time| time.to_rfc3339()),
            plan_approved_at: plan.approved_at.map(|time| time.to_rfc3339()),
            plan_revision_feedback: plan.revision_feedback.clone(),
            plan_revision_requested_at: plan.revision_requested_at.map(|time| time.to_rfc3339()),
        })
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_task(&self, task_id: TaskId) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<TaskRecordRow>>::filter(TaskRecordRow::fields().id().eq(task_id.to_string()))
            .delete()
            .exec(&mut tx)
            .await?;
        Query::<List<TaskReviewRecord>>::filter(
            TaskReviewRecord::fields().task_id().eq(task_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        Query::<List<PlanHistoryRecord>>::filter(
            PlanHistoryRecord::fields()
                .task_id()
                .eq(task_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn append_task_review(&self, review: &TaskReview) -> Result<()> {
        let mut db = self.db.clone();
        Query::<List<TaskReviewRecord>>::filter(
            TaskReviewRecord::fields().id().eq(review.id.to_string()),
        )
        .delete()
        .exec(&mut db)
        .await?;
        toasty::create!(TaskReviewRecord {
            id: review.id.to_string(),
            task_id: review.task_id.to_string(),
            reviewer_agent_id: review.reviewer_agent_id.to_string(),
            round: u64_to_i64(review.round),
            passed: review.passed,
            findings: review.findings.clone(),
            summary: review.summary.clone(),
            created_at: review.created_at.to_rfc3339(),
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn save_plan_history_entry(
        &self,
        task_id: TaskId,
        entry: &PlanHistoryEntry,
    ) -> Result<()> {
        let mut db = self.db.clone();
        toasty::create!(PlanHistoryRecord {
            id: Uuid::new_v4().to_string(),
            task_id: task_id.to_string(),
            version: u64_to_i64(entry.version),
            title: entry.title.clone(),
            markdown: entry.markdown.clone(),
            saved_at: entry.saved_at.map(|time| time.to_rfc3339()),
            saved_by_agent_id: entry.saved_by_agent_id.map(|id| id.to_string()),
            revision_feedback: entry.revision_feedback.clone(),
            revision_requested_at: entry.revision_requested_at.map(|time| time.to_rfc3339()),
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn load_plan_history(&self, task_id: TaskId) -> Result<Vec<PlanHistoryEntry>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<PlanHistoryRecord>>::filter(
            PlanHistoryRecord::fields()
                .task_id()
                .eq(task_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.sort_by_key(|row| row.version);
        rows.into_iter()
            .map(|row| {
                Ok(PlanHistoryEntry {
                    version: i64_to_u64(row.version),
                    title: row.title,
                    markdown: row.markdown,
                    saved_at: row.saved_at.as_deref().map(parse_utc).transpose()?,
                    saved_by_agent_id: row
                        .saved_by_agent_id
                        .as_deref()
                        .map(parse_agent_id)
                        .transpose()?,
                    revision_feedback: row.revision_feedback,
                    revision_requested_at: row
                        .revision_requested_at
                        .as_deref()
                        .map(parse_utc)
                        .transpose()?,
                })
            })
            .collect()
    }

    pub async fn load_task_reviews(&self, task_id: TaskId) -> Result<Vec<TaskReview>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<TaskReviewRecord>>::filter(
            TaskReviewRecord::fields().task_id().eq(task_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.sort_by(|left, right| {
            left.round
                .cmp(&right.round)
                .then_with(|| left.created_at.cmp(&right.created_at))
        });
        rows.into_iter()
            .map(TaskReviewRecord::into_review)
            .collect()
    }
}
