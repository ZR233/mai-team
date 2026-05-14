use std::future::Future;

use mai_protocol::{
    AgentId, AgentStatus, AgentSummary, ProjectId, ProjectReviewOutcome, ProjectReviewRunDetail,
    ProjectReviewRunStatus, ProjectReviewRunSummary, ProjectReviewStatus, TurnId, now,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::ProjectReviewCycleResult;
use super::runs::FinishReviewRun;
use super::state::ReviewStateUpdate;
use crate::{Result, RuntimeError};

/// Provides the review cycle dependencies needed to sync a project workspace,
/// run a reviewer agent turn, persist run state, and clean up afterwards.
pub(crate) trait ProjectReviewCycleOps: Send + Sync {
    fn set_project_review_state(
        &self,
        project_id: ProjectId,
        status: ProjectReviewStatus,
        update: ReviewStateUpdate,
    ) -> impl Future<Output = Result<mai_protocol::ProjectSummary>> + Send;

    fn save_project_review_run_status(
        &self,
        summary: ProjectReviewRunSummary,
    ) -> impl Future<Output = Result<()>> + Send;

    fn load_project_review_run(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
    ) -> impl Future<Output = Result<Option<ProjectReviewRunDetail>>> + Send;

    fn update_project_review_run_turn(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
        reviewer_agent_id: AgentId,
        turn_id: TurnId,
    ) -> impl Future<Output = Result<()>> + Send;

    fn finish_project_review_run(
        &self,
        request: FinishReviewRun,
    ) -> impl Future<Output = Result<()>> + Send;

    fn sync_project_review_repo(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<()>> + Send;

    fn refresh_project_skills_from_review_workspace(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<()>> + Send;

    fn spawn_project_reviewer_agent(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<AgentSummary>> + Send;

    fn project_reviewer_initial_message(
        &self,
        project_id: ProjectId,
        reviewer_id: AgentId,
        target_pr: Option<u64>,
    ) -> impl Future<Output = Result<String>> + Send;

    fn start_reviewer_turn(
        &self,
        reviewer_id: AgentId,
        message: String,
    ) -> impl Future<Output = Result<TurnId>> + Send;

    fn wait_agent_until_complete_with_cancel(
        &self,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> impl Future<Output = Result<AgentSummary>> + Send;

    fn reviewer_final_response(
        &self,
        reviewer_id: AgentId,
    ) -> impl Future<Output = Result<String>> + Send;

    fn delete_agent(&self, agent_id: AgentId) -> impl Future<Output = Result<()>> + Send;
}

pub(crate) async fn run_project_review_once(
    ops: &impl ProjectReviewCycleOps,
    project_id: ProjectId,
    cancellation_token: CancellationToken,
    target_pr: Option<u64>,
) -> Result<ProjectReviewCycleResult> {
    let run_id = Uuid::new_v4();
    ops.set_project_review_state(
        project_id,
        ProjectReviewStatus::Syncing,
        ReviewStateUpdate::default(),
    )
    .await?;
    ops.save_project_review_run_status(ProjectReviewRunSummary {
        id: run_id,
        project_id,
        reviewer_agent_id: None,
        turn_id: None,
        started_at: now(),
        finished_at: None,
        status: ProjectReviewRunStatus::Syncing,
        outcome: None,
        pr: target_pr,
        summary: None,
        error: None,
    })
    .await?;
    if let Err(err) = ops.sync_project_review_repo(project_id).await {
        let error = err.to_string();
        ops.finish_project_review_run(FinishReviewRun {
            run_id,
            project_id,
            reviewer_agent_id: None,
            turn_id: None,
            status: ProjectReviewRunStatus::Failed,
            outcome: Some(ProjectReviewOutcome::Failed),
            pr: target_pr,
            summary_text: None,
            error: Some(error),
        })
        .await?;
        return Err(err);
    }
    if let Err(err) = ops
        .refresh_project_skills_from_review_workspace(project_id)
        .await
    {
        let error = err.to_string();
        ops.finish_project_review_run(FinishReviewRun {
            run_id,
            project_id,
            reviewer_agent_id: None,
            turn_id: None,
            status: ProjectReviewRunStatus::Failed,
            outcome: Some(ProjectReviewOutcome::Failed),
            pr: target_pr,
            summary_text: None,
            error: Some(error),
        })
        .await?;
        return Err(err);
    }
    if cancellation_token.is_cancelled() {
        ops.finish_project_review_run(FinishReviewRun {
            run_id,
            project_id,
            reviewer_agent_id: None,
            turn_id: None,
            status: ProjectReviewRunStatus::Cancelled,
            outcome: None,
            pr: target_pr,
            summary_text: None,
            error: Some("review cancelled".to_string()),
        })
        .await?;
        return Err(RuntimeError::TurnCancelled);
    }
    let reviewer = match ops.spawn_project_reviewer_agent(project_id).await {
        Ok(reviewer) => reviewer,
        Err(err) => {
            ops.finish_project_review_run(FinishReviewRun {
                run_id,
                project_id,
                reviewer_agent_id: None,
                turn_id: None,
                status: ProjectReviewRunStatus::Failed,
                outcome: Some(ProjectReviewOutcome::Failed),
                pr: target_pr,
                summary_text: None,
                error: Some(err.to_string()),
            })
            .await?;
            return Err(err);
        }
    };
    let reviewer_id = reviewer.id;
    ops.set_project_review_state(
        project_id,
        ProjectReviewStatus::Running,
        ReviewStateUpdate {
            current_reviewer_agent_id: Some(reviewer_id),
            ..Default::default()
        },
    )
    .await?;
    let started_at = ops
        .load_project_review_run(project_id, run_id)
        .await?
        .map(|run| run.summary.started_at)
        .unwrap_or_else(now);
    ops.save_project_review_run_status(ProjectReviewRunSummary {
        id: run_id,
        project_id,
        reviewer_agent_id: Some(reviewer_id),
        turn_id: None,
        started_at,
        finished_at: None,
        status: ProjectReviewRunStatus::Running,
        outcome: None,
        pr: target_pr,
        summary: None,
        error: None,
    })
    .await?;
    let cycle_result = async {
        let message = ops
            .project_reviewer_initial_message(project_id, reviewer_id, target_pr)
            .await?;
        let turn_id = ops.start_reviewer_turn(reviewer_id, message).await?;
        ops.update_project_review_run_turn(project_id, run_id, reviewer_id, turn_id)
            .await?;
        let summary = ops
            .wait_agent_until_complete_with_cancel(reviewer_id, &cancellation_token)
            .await?;
        if summary.status == AgentStatus::Cancelled && cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        if let Some(result) = super::project_review_cycle_result_for_reviewer_status(&summary) {
            return Ok(result);
        }
        let response = ops.reviewer_final_response(reviewer_id).await?;
        super::parse_project_review_cycle_report(&response)
    }
    .await;
    let turn_id = ops
        .load_project_review_run(project_id, run_id)
        .await?
        .and_then(|run| run.summary.turn_id);
    let (status, outcome, pr, summary, error) = match &cycle_result {
        Ok(result) => {
            let status = if result.outcome == ProjectReviewOutcome::Failed {
                ProjectReviewRunStatus::Failed
            } else {
                ProjectReviewRunStatus::Completed
            };
            (
                status,
                Some(result.outcome.clone()),
                result.pr,
                result.summary.clone(),
                result.error.clone(),
            )
        }
        Err(RuntimeError::TurnCancelled) if cancellation_token.is_cancelled() => (
            ProjectReviewRunStatus::Cancelled,
            None,
            None,
            None,
            Some("review cancelled".to_string()),
        ),
        Err(err) => (
            ProjectReviewRunStatus::Failed,
            Some(ProjectReviewOutcome::Failed),
            None,
            None,
            Some(err.to_string()),
        ),
    };
    let _ = ops
        .finish_project_review_run(FinishReviewRun {
            run_id,
            project_id,
            reviewer_agent_id: Some(reviewer_id),
            turn_id,
            status,
            outcome,
            pr,
            summary_text: summary,
            error,
        })
        .await;
    let _ = ops.delete_agent(reviewer_id).await;
    ops.set_project_review_state(
        project_id,
        ProjectReviewStatus::Idle,
        ReviewStateUpdate::default(),
    )
    .await?;
    cycle_result
}
