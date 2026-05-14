use std::future::Future;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use mai_protocol::{AgentId, ProjectId, ProjectReviewOutcome, ProjectReviewStatus, ProjectSummary};

use crate::state::ProjectRecord;
use crate::{Result, now};

#[derive(Default)]
pub(crate) struct ReviewStateUpdate {
    pub(crate) current_reviewer_agent_id: Option<AgentId>,
    pub(crate) next_review_at: Option<DateTime<Utc>>,
    pub(crate) outcome: Option<ProjectReviewOutcome>,
    pub(crate) summary_text: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) force_disabled: bool,
}

/// Provides project persistence and event publishing needed to transition the
/// project review status without exposing the full runtime.
pub(crate) trait ProjectReviewStateOps: Send + Sync {
    fn project(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<Arc<ProjectRecord>>> + Send;

    fn save_project(&self, project: ProjectSummary) -> impl Future<Output = Result<()>> + Send;

    fn publish_project_updated(&self, project: ProjectSummary) -> impl Future<Output = ()> + Send;
}

pub(crate) async fn set_project_review_state(
    ops: &impl ProjectReviewStateOps,
    project_id: ProjectId,
    status: ProjectReviewStatus,
    update: ReviewStateUpdate,
) -> Result<ProjectSummary> {
    let project = ops.project(project_id).await?;
    let updated = {
        let mut summary = project.summary.write().await;
        summary.review_status = status;
        summary.current_reviewer_agent_id = update.current_reviewer_agent_id;
        summary.next_review_at = update.next_review_at;
        if update.current_reviewer_agent_id.is_some() {
            summary.last_review_started_at = Some(now());
            summary.last_review_finished_at = None;
        } else if update.outcome.is_some() || update.error.is_some() {
            summary.last_review_finished_at = Some(now());
        }
        if let Some(outcome) = update.outcome {
            summary.last_review_outcome = Some(outcome);
        }
        summary.review_last_error = update.error;
        if update.force_disabled {
            summary.auto_review_enabled = false;
        }
        summary.updated_at = now();
        summary.clone()
    };
    let _ = &update.summary_text;
    ops.save_project(updated.clone()).await?;
    ops.publish_project_updated(updated.clone()).await;
    Ok(updated)
}
