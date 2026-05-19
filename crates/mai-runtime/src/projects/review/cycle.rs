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

const REVIEWER_FINAL_JSON_REPAIR_PROMPT: &str = "The previous response did not include the required final JSON object, so the project review scheduler could not record the result. Continue from the existing review state. If the GitHub review has already been submitted, do not submit a duplicate review. If it has not been submitted yet, submit it now using the available GitHub API tool. Then reply with only one JSON object matching this schema exactly and no surrounding text: {\"outcome\":\"review_submitted|failed\",\"pr\":123|null,\"summary\":\"short result\",\"error\":null|\"failure reason\"}";

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

    fn sync_project_cache_repo(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<()>> + Send;

    fn refresh_project_skills_from_agent_workspace(
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
        token_usage: Default::default(),
    })
    .await?;
    if let Err(err) = ops.sync_project_cache_repo(project_id).await {
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
        .refresh_project_skills_from_agent_workspace(project_id)
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
        token_usage: Default::default(),
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
        parse_reviewer_final_response(ops, reviewer_id, &cancellation_token).await
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

async fn parse_reviewer_final_response(
    ops: &impl ProjectReviewCycleOps,
    reviewer_id: AgentId,
    cancellation_token: &CancellationToken,
) -> Result<ProjectReviewCycleResult> {
    let response = ops.reviewer_final_response(reviewer_id).await?;
    match super::parse_project_review_cycle_report(&response) {
        Ok(result) => Ok(result),
        Err(first_err) => {
            let turn_id = ops
                .start_reviewer_turn(reviewer_id, REVIEWER_FINAL_JSON_REPAIR_PROMPT.to_string())
                .await?;
            tracing::warn!(
                reviewer_id = %reviewer_id,
                turn_id = %turn_id,
                error = %first_err,
                "project reviewer final JSON missing or invalid; requesting one repair turn"
            );
            let summary = ops
                .wait_agent_until_complete_with_cancel(reviewer_id, cancellation_token)
                .await?;
            if summary.status == AgentStatus::Cancelled && cancellation_token.is_cancelled() {
                return Err(RuntimeError::TurnCancelled);
            }
            if let Some(result) = super::project_review_cycle_result_for_reviewer_status(&summary) {
                return Ok(result);
            }
            let repaired_response = ops.reviewer_final_response(reviewer_id).await?;
            super::parse_project_review_cycle_report(&repaired_response)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use mai_protocol::{ProjectCloneStatus, ProjectStatus, TokenUsage};
    use pretty_assertions::assert_eq;
    use tokio::sync::Mutex;

    use super::*;

    #[derive(Clone)]
    struct FakeCycleOps {
        reviewer: AgentSummary,
        responses: Arc<Mutex<Vec<String>>>,
        started_messages: Arc<Mutex<Vec<String>>>,
        finished_runs: Arc<Mutex<Vec<FinishReviewRun>>>,
        run_summary: Arc<Mutex<Option<ProjectReviewRunSummary>>>,
    }

    impl FakeCycleOps {
        fn new(project_id: ProjectId, reviewer_id: AgentId, responses: Vec<String>) -> Self {
            Self {
                reviewer: test_agent_summary(project_id, reviewer_id),
                responses: Arc::new(Mutex::new(responses)),
                started_messages: Arc::new(Mutex::new(Vec::new())),
                finished_runs: Arc::new(Mutex::new(Vec::new())),
                run_summary: Arc::new(Mutex::new(None)),
            }
        }
    }

    impl ProjectReviewCycleOps for FakeCycleOps {
        async fn set_project_review_state(
            &self,
            project_id: ProjectId,
            status: ProjectReviewStatus,
            _update: ReviewStateUpdate,
        ) -> Result<mai_protocol::ProjectSummary> {
            Ok(test_project_summary(project_id, self.reviewer.id, status))
        }

        async fn save_project_review_run_status(
            &self,
            summary: ProjectReviewRunSummary,
        ) -> Result<()> {
            *self.run_summary.lock().await = Some(summary);
            Ok(())
        }

        async fn load_project_review_run(
            &self,
            _project_id: ProjectId,
            _run_id: Uuid,
        ) -> Result<Option<ProjectReviewRunDetail>> {
            let Some(summary) = self.run_summary.lock().await.clone() else {
                return Ok(None);
            };
            Ok(Some(ProjectReviewRunDetail {
                summary,
                messages: Vec::new(),
                events: Vec::new(),
            }))
        }

        async fn update_project_review_run_turn(
            &self,
            _project_id: ProjectId,
            _run_id: Uuid,
            reviewer_agent_id: AgentId,
            turn_id: TurnId,
        ) -> Result<()> {
            let mut run_summary = self.run_summary.lock().await;
            if let Some(summary) = run_summary.as_mut() {
                summary.reviewer_agent_id = Some(reviewer_agent_id);
                summary.turn_id = Some(turn_id);
            }
            Ok(())
        }

        async fn finish_project_review_run(&self, request: FinishReviewRun) -> Result<()> {
            self.finished_runs.lock().await.push(request);
            Ok(())
        }

        async fn sync_project_cache_repo(&self, _project_id: ProjectId) -> Result<()> {
            Ok(())
        }

        async fn refresh_project_skills_from_agent_workspace(
            &self,
            _project_id: ProjectId,
        ) -> Result<()> {
            Ok(())
        }

        async fn spawn_project_reviewer_agent(
            &self,
            _project_id: ProjectId,
        ) -> Result<AgentSummary> {
            Ok(self.reviewer.clone())
        }

        async fn project_reviewer_initial_message(
            &self,
            _project_id: ProjectId,
            _reviewer_id: AgentId,
            target_pr: Option<u64>,
        ) -> Result<String> {
            Ok(format!("review target {target_pr:?}"))
        }

        async fn start_reviewer_turn(
            &self,
            _reviewer_id: AgentId,
            message: String,
        ) -> Result<TurnId> {
            self.started_messages.lock().await.push(message);
            Ok(Uuid::new_v4())
        }

        async fn wait_agent_until_complete_with_cancel(
            &self,
            _agent_id: AgentId,
            _cancellation_token: &CancellationToken,
        ) -> Result<AgentSummary> {
            Ok(self.reviewer.clone())
        }

        async fn reviewer_final_response(&self, _reviewer_id: AgentId) -> Result<String> {
            let mut responses = self.responses.lock().await;
            if responses.is_empty() {
                return Err(RuntimeError::InvalidInput(
                    "missing fake reviewer response".to_string(),
                ));
            }
            Ok(responses.remove(0))
        }

        async fn delete_agent(&self, _agent_id: AgentId) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn review_cycle_repairs_missing_final_json_once() {
        let project_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let ops = FakeCycleOps::new(
            project_id,
            reviewer_id,
            vec![
                "Now let me submit the review.".to_string(),
                r#"{"outcome":"review_submitted","pr":726,"summary":"已提交 review","error":null}"#
                    .to_string(),
            ],
        );

        let result = run_project_review_once(&ops, project_id, CancellationToken::new(), Some(726))
            .await
            .expect("review result");

        assert_eq!(ProjectReviewOutcome::ReviewSubmitted, result.outcome);
        assert_eq!(Some(726), result.pr);
        assert_eq!(Some("已提交 review"), result.summary.as_deref());
        let messages = ops.started_messages.lock().await.clone();
        assert_eq!(2, messages.len());
        assert!(messages[1].contains("previous response did not include"));
        assert!(messages[1].contains("do not submit a duplicate review"));
        let finished = ops.finished_runs.lock().await.clone();
        assert_eq!(1, finished.len());
        assert_eq!(ProjectReviewRunStatus::Completed, finished[0].status);
        assert_eq!(
            Some(ProjectReviewOutcome::ReviewSubmitted),
            finished[0].outcome
        );
        assert_eq!(Some(726), finished[0].pr);
    }

    fn test_project_summary(
        project_id: ProjectId,
        reviewer_id: AgentId,
        review_status: ProjectReviewStatus,
    ) -> mai_protocol::ProjectSummary {
        let timestamp = Utc::now();
        mai_protocol::ProjectSummary {
            id: project_id,
            name: "owner/repo".to_string(),
            status: ProjectStatus::Ready,
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_full_name: "owner/repo".to_string(),
            git_account_id: Some("account-1".to_string()),
            repository_id: 1,
            installation_id: 0,
            installation_account: "owner".to_string(),
            branch: "main".to_string(),
            docker_image: "ubuntu:latest".to_string(),
            clone_status: ProjectCloneStatus::Ready,
            maintainer_agent_id: reviewer_id,
            created_at: timestamp,
            updated_at: timestamp,
            last_error: None,
            auto_review_enabled: true,
            reviewer_extra_prompt: None,
            review_status,
            current_reviewer_agent_id: None,
            last_review_started_at: None,
            last_review_finished_at: None,
            next_review_at: None,
            last_review_outcome: None,
            review_last_error: None,
        }
    }

    fn test_agent_summary(project_id: ProjectId, reviewer_id: AgentId) -> AgentSummary {
        let timestamp = Utc::now();
        AgentSummary {
            id: reviewer_id,
            parent_id: None,
            task_id: None,
            project_id: Some(project_id),
            role: Some(mai_protocol::AgentRole::Reviewer),
            name: "reviewer".to_string(),
            status: AgentStatus::Completed,
            container_id: Some("container".to_string()),
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "mock".to_string(),
            provider_name: "Mock".to_string(),
            model: "mock-model".to_string(),
            reasoning_effort: Some("medium".to_string()),
            created_at: timestamp,
            updated_at: timestamp,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        }
    }
}
