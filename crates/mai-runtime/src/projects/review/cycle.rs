use std::future::Future;

use mai_protocol::{AgentId, ProjectId, ProjectReviewRunDetail, ProjectReviewRunSummary, TurnId};
#[cfg(test)]
use mai_protocol::{ProjectReviewOutcome, ProjectReviewRunStatus, ProjectReviewStatus, now};
use pl_core::{AgentWaitResult, TurnOutcomeKind};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::ProjectReviewCycleResult;
use super::reviewer::PreparedProjectReviewer;
use super::runs::FinishReviewRun;
#[cfg(test)]
use super::state::{ReviewStateUpdate, ReviewerAgentUpdate};
use super::target::ProjectReviewRequest;
use crate::{Result, RuntimeError};

const REVIEWER_FINAL_JSON_REPAIR_PROMPT: &str = "The previous response did not include the required final JSON object, so the project review scheduler could not record the result. Continue from the existing review state. If the GitHub review has already been submitted, do not submit a duplicate review. If it has not been submitted yet, submit it now using the available GitHub API tool. Then reply with only one JSON object matching this schema exactly and no surrounding text: {\"outcome\":\"review_submitted|failed\",\"review_event\":\"approve|request_changes|comment\"|null,\"pr\":123|null,\"summary\":\"short result\",\"error\":null|\"failure reason\"}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReviewerProgress {
    pub(crate) revision: u64,
    pub(crate) inactivity_timeout: std::time::Duration,
}

/// Provides the review cycle dependencies needed to sync a project workspace,
/// run a reviewer agent turn, persist run state, and clean up afterwards.
pub(crate) trait ProjectReviewCycleOps: Send + Sync {
    #[cfg(test)]
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

    fn prepare_project_reviewer(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
        request: ProjectReviewRequest,
    ) -> impl Future<Output = Result<PreparedProjectReviewer>> + Send;

    fn project_reviewer_initial_message(
        &self,
        project_id: ProjectId,
        reviewer_id: AgentId,
        target: super::target::ResolvedProjectReviewTarget,
        project_revision: crate::projects::workspace::ProjectRepositoryRevision,
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
    ) -> impl Future<Output = Result<AgentWaitResult>> + Send;

    fn reviewer_progress(
        &self,
        reviewer_id: AgentId,
    ) -> impl Future<Output = Result<ReviewerProgress>> + Send;

    fn cancel_reviewer_turn(
        &self,
        reviewer_id: AgentId,
        turn_id: TurnId,
    ) -> impl Future<Output = Result<()>> + Send;

    fn reviewer_final_response(
        &self,
        reviewer_id: AgentId,
    ) -> impl Future<Output = Result<String>> + Send;

    fn reviewer_target_is_stale(
        &self,
        reviewer_id: AgentId,
    ) -> impl Future<Output = Result<bool>> + Send;

    #[cfg(test)]
    fn delete_agent(&self, agent_id: AgentId) -> impl Future<Output = Result<()>> + Send;
}

#[cfg(test)]
pub(crate) async fn run_project_review_once(
    ops: &impl ProjectReviewCycleOps,
    project_id: ProjectId,
    cancellation_token: CancellationToken,
    request: ProjectReviewRequest,
) -> Result<ProjectReviewCycleResult> {
    let run_id = Uuid::new_v4();
    let target_pr = Some(request.pr);
    ops.set_project_review_state(
        project_id,
        ProjectReviewStatus::Syncing,
        ReviewStateUpdate::default(),
    )
    .await?;
    ops.save_project_review_run_status(ProjectReviewRunSummary {
        id: run_id,
        job_id: None,
        attempt_index: 1,
        project_id,
        reviewer_agent_id: None,
        turn_id: None,
        started_at: now(),
        finished_at: None,
        status: ProjectReviewRunStatus::Syncing,
        outcome: None,
        review_event: None,
        pr: target_pr,
        summary: None,
        error: None,
        failure: None,
        token_usage: Default::default(),
    })
    .await?;
    if cancellation_token.is_cancelled() {
        ops.finish_project_review_run(FinishReviewRun {
            run_id,
            project_id,
            reviewer_agent_id: None,
            turn_id: None,
            status: ProjectReviewRunStatus::Cancelled,
            outcome: None,
            review_event: None,
            pr: target_pr,
            summary_text: None,
            error: Some("review cancelled".to_string()),
            failure: None,
        })
        .await?;
        return Err(RuntimeError::TurnCancelled);
    }
    let prepared = match ops
        .prepare_project_reviewer(project_id, run_id, request)
        .await
    {
        Ok(prepared) => prepared,
        Err(err) => {
            ops.finish_project_review_run(FinishReviewRun {
                run_id,
                project_id,
                reviewer_agent_id: None,
                turn_id: None,
                status: ProjectReviewRunStatus::Failed,
                outcome: Some(ProjectReviewOutcome::Failed),
                review_event: None,
                pr: target_pr,
                summary_text: None,
                error: Some(err.to_string()),
                failure: None,
            })
            .await?;
            return Err(err);
        }
    };
    let reviewer = &prepared.agent;
    let reviewer_id = reviewer.id;
    if let Err(err) = ops
        .set_project_review_state(
            project_id,
            ProjectReviewStatus::Running,
            ReviewStateUpdate {
                current_reviewer_agent_id: ReviewerAgentUpdate::Set(reviewer_id),
                ..Default::default()
            },
        )
        .await
    {
        let error = err.to_string();
        finish_spawned_reviewer_error(ops, project_id, run_id, reviewer_id, target_pr, error).await;
        return Err(err);
    }
    let started_at = match ops.load_project_review_run(project_id, run_id).await {
        Ok(Some(run)) => run.summary.started_at,
        Ok(None) => now(),
        Err(err) => {
            let error = err.to_string();
            finish_spawned_reviewer_error(ops, project_id, run_id, reviewer_id, target_pr, error)
                .await;
            return Err(err);
        }
    };
    if let Err(err) = ops
        .save_project_review_run_status(ProjectReviewRunSummary {
            id: run_id,
            job_id: None,
            attempt_index: 1,
            project_id,
            reviewer_agent_id: Some(reviewer_id),
            turn_id: None,
            started_at,
            finished_at: None,
            status: ProjectReviewRunStatus::Running,
            outcome: None,
            review_event: None,
            pr: target_pr,
            summary: None,
            error: None,
            failure: None,
            token_usage: Default::default(),
        })
        .await
    {
        let error = err.to_string();
        finish_spawned_reviewer_error(ops, project_id, run_id, reviewer_id, target_pr, error).await;
        return Err(err);
    }
    let cycle_result = async {
        let message = ops
            .project_reviewer_initial_message(
                project_id,
                reviewer_id,
                prepared.target.clone(),
                prepared.project_revision.clone(),
            )
            .await?;
        let turn_id = ops.start_reviewer_turn(reviewer_id, message).await?;
        ops.update_project_review_run_turn(project_id, run_id, reviewer_id, turn_id)
            .await?;
        let wait_result = ops
            .wait_agent_until_complete_with_cancel(reviewer_id, &cancellation_token)
            .await?;
        if last_turn_cancelled(&wait_result) && cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        if let Some(result) = super::project_review_cycle_result_for_wait_result(&wait_result) {
            return Ok(result);
        }
        parse_reviewer_final_response(ops, reviewer_id, &cancellation_token).await
    }
    .await;
    let cycle_result = match ops.reviewer_target_is_stale(reviewer_id).await {
        Ok(true) => Err(RuntimeError::InvalidInput(format!(
            "{} for PR #{}",
            super::REVIEW_TARGET_HEAD_CHANGED,
            prepared.target.pr
        ))),
        Ok(false) => cycle_result.and_then(|result| {
            match result.pr {
                Some(reported_pr) if reported_pr != prepared.target.pr => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "reviewer reported PR #{reported_pr} while the prepared target is PR #{}",
                        prepared.target.pr
                    )));
                }
                Some(_) | None => {}
            }
            Ok(result)
        }),
        Err(error) => Err(error),
    };
    let turn_id = match ops.load_project_review_run(project_id, run_id).await {
        Ok(run) => run.and_then(|run| run.summary.turn_id),
        Err(err) => {
            tracing::warn!(
                project_id = %project_id,
                run_id = %run_id,
                reviewer_id = %reviewer_id,
                "failed to reload project review run before cleanup: {err}"
            );
            None
        }
    };
    let (status, outcome, review_event, summary, error, failure) = match &cycle_result {
        Ok(result) => {
            let status = if result.outcome == ProjectReviewOutcome::Failed {
                ProjectReviewRunStatus::Failed
            } else {
                ProjectReviewRunStatus::Completed
            };
            (
                status,
                Some(result.outcome.clone()),
                result.review_event.clone(),
                result.summary.clone(),
                result.error.clone(),
                result.failure.clone(),
            )
        }
        Err(RuntimeError::TurnCancelled) if cancellation_token.is_cancelled() => (
            ProjectReviewRunStatus::Cancelled,
            None,
            None,
            None,
            Some("review cancelled".to_string()),
            None,
        ),
        Err(err) => (
            ProjectReviewRunStatus::Failed,
            Some(ProjectReviewOutcome::Failed),
            None,
            None,
            Some(err.to_string()),
            None,
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
            review_event,
            pr: target_pr,
            summary_text: summary,
            error,
            failure,
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

#[cfg(test)]
async fn finish_spawned_reviewer_error(
    ops: &impl ProjectReviewCycleOps,
    project_id: ProjectId,
    run_id: Uuid,
    reviewer_id: AgentId,
    target_pr: Option<u64>,
    error: String,
) {
    let _ = ops
        .finish_project_review_run(FinishReviewRun {
            run_id,
            project_id,
            reviewer_agent_id: Some(reviewer_id),
            turn_id: None,
            status: ProjectReviewRunStatus::Failed,
            outcome: Some(ProjectReviewOutcome::Failed),
            review_event: None,
            pr: target_pr,
            summary_text: None,
            error: Some(error),
            failure: None,
        })
        .await;
    let _ = ops.delete_agent(reviewer_id).await;
    let _ = ops
        .set_project_review_state(
            project_id,
            ProjectReviewStatus::Idle,
            ReviewStateUpdate::default(),
        )
        .await;
}

pub(crate) async fn parse_reviewer_final_response(
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
            let wait_result = ops
                .wait_agent_until_complete_with_cancel(reviewer_id, cancellation_token)
                .await?;
            if last_turn_cancelled(&wait_result) && cancellation_token.is_cancelled() {
                return Err(RuntimeError::TurnCancelled);
            }
            if let Some(result) = super::project_review_cycle_result_for_wait_result(&wait_result) {
                return Ok(result);
            }
            let repaired_response = ops.reviewer_final_response(reviewer_id).await?;
            super::parse_project_review_cycle_report(&repaired_response)
        }
    }
}

pub(crate) fn last_turn_cancelled(wait_result: &AgentWaitResult) -> bool {
    wait_result
        .last_turn
        .as_ref()
        .is_some_and(|turn| turn.kind == TurnOutcomeKind::Cancelled)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use mai_protocol::{
        AgentLastTurn, AgentResourceState, AgentRuntimeState, AgentState, AgentSummary,
        AgentTurnOutcomeKind, ProjectCloneStatus, ProjectReviewDecision, ProjectStatus, TokenUsage,
    };
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
        deleted_agents: Arc<Mutex<Vec<AgentId>>>,
        operations: Arc<Mutex<Vec<&'static str>>>,
        fail_running_state: bool,
        target_stale: bool,
    }

    impl FakeCycleOps {
        fn new(project_id: ProjectId, reviewer_id: AgentId, responses: Vec<String>) -> Self {
            Self {
                reviewer: test_agent_summary(project_id, reviewer_id),
                responses: Arc::new(Mutex::new(responses)),
                started_messages: Arc::new(Mutex::new(Vec::new())),
                finished_runs: Arc::new(Mutex::new(Vec::new())),
                run_summary: Arc::new(Mutex::new(None)),
                deleted_agents: Arc::new(Mutex::new(Vec::new())),
                operations: Arc::new(Mutex::new(Vec::new())),
                fail_running_state: false,
                target_stale: false,
            }
        }

        fn with_running_state_failure(mut self) -> Self {
            self.fail_running_state = true;
            self
        }

        fn with_stale_target(mut self) -> Self {
            self.target_stale = true;
            self
        }
    }

    impl ProjectReviewCycleOps for FakeCycleOps {
        async fn set_project_review_state(
            &self,
            project_id: ProjectId,
            status: ProjectReviewStatus,
            _update: ReviewStateUpdate,
        ) -> Result<mai_protocol::ProjectSummary> {
            if self.fail_running_state && status == ProjectReviewStatus::Running {
                return Err(RuntimeError::InvalidInput(
                    "failed to persist running review state".to_string(),
                ));
            }
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

        async fn prepare_project_reviewer(
            &self,
            _project_id: ProjectId,
            _run_id: Uuid,
            request: ProjectReviewRequest,
        ) -> Result<PreparedProjectReviewer> {
            self.operations.lock().await.extend([
                "resolve_review_target",
                "sync_project_repository",
                "snapshot_default_branch_context",
                "spawn_project_reviewer",
            ]);
            Ok(PreparedProjectReviewer {
                agent: self.reviewer.clone(),
                target: super::super::target::ResolvedProjectReviewTarget {
                    pr: request.pr,
                    head_sha: "head-sha".to_string(),
                },
                project_revision: crate::projects::workspace::ProjectRepositoryRevision {
                    branch: "main".to_string(),
                    base_sha: "base-sha".to_string(),
                },
            })
        }

        async fn project_reviewer_initial_message(
            &self,
            _project_id: ProjectId,
            _reviewer_id: AgentId,
            target: super::super::target::ResolvedProjectReviewTarget,
            _project_revision: crate::projects::workspace::ProjectRepositoryRevision,
        ) -> Result<String> {
            Ok(format!("review target {}", target.pr))
        }

        async fn start_reviewer_turn(
            &self,
            _reviewer_id: AgentId,
            message: String,
        ) -> Result<TurnId> {
            self.operations.lock().await.push("start_reviewer_turn");
            self.started_messages.lock().await.push(message);
            Ok(Uuid::new_v4())
        }

        async fn wait_agent_until_complete_with_cancel(
            &self,
            _agent_id: AgentId,
            _cancellation_token: &CancellationToken,
        ) -> Result<AgentWaitResult> {
            Ok(completed_wait_result())
        }

        async fn reviewer_progress(&self, _reviewer_id: AgentId) -> Result<ReviewerProgress> {
            Ok(ReviewerProgress {
                revision: self.reviewer.state.runtime.revision,
                inactivity_timeout: std::time::Duration::from_secs(600),
            })
        }

        async fn cancel_reviewer_turn(
            &self,
            _reviewer_id: AgentId,
            _turn_id: TurnId,
        ) -> Result<()> {
            Ok(())
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

        async fn reviewer_target_is_stale(&self, _reviewer_id: AgentId) -> Result<bool> {
            Ok(self.target_stale)
        }

        async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
            self.deleted_agents.lock().await.push(agent_id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn review_cycle_refreshes_default_branch_context_before_reviewer_turn() {
        let project_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let ops = FakeCycleOps::new(
            project_id,
            reviewer_id,
            vec![r#"{"outcome":"failed","review_event":null,"pr":726,"summary":"done","error":"stop"}"#
                .to_string()],
        );

        let result =
            run_project_review_once(&ops, project_id, CancellationToken::new(), request(726))
                .await
                .expect("review result");

        assert_eq!(ProjectReviewOutcome::Failed, result.outcome);
        assert_eq!(
            vec![
                "resolve_review_target",
                "sync_project_repository",
                "snapshot_default_branch_context",
                "spawn_project_reviewer",
                "start_reviewer_turn",
            ],
            *ops.operations.lock().await
        );
    }

    #[tokio::test]
    async fn durable_wait_outcome_wins_over_stale_agent_summary_projection() {
        let project_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let mut ops = FakeCycleOps::new(
            project_id,
            reviewer_id,
            vec![r#"{"outcome":"review_submitted","review_event":"approve","pr":726,"summary":"done","error":null}"#.to_string()],
        );
        ops.reviewer.state.runtime.last_turn = None;

        let result =
            run_project_review_once(&ops, project_id, CancellationToken::new(), request(726))
                .await
                .expect("review result");

        assert_eq!(ProjectReviewOutcome::ReviewSubmitted, result.outcome);
    }

    #[tokio::test]
    async fn review_cycle_deletes_reviewer_when_running_state_fails() {
        let project_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let ops = FakeCycleOps::new(
            project_id,
            reviewer_id,
            vec![r#"{"outcome":"review_submitted","review_event":"approve","pr":726,"summary":"ok","error":null}"#
                .to_string()],
        )
        .with_running_state_failure();

        let result =
            run_project_review_once(&ops, project_id, CancellationToken::new(), request(726)).await;

        assert!(result.is_err());
        assert_eq!(vec![reviewer_id], *ops.deleted_agents.lock().await);
        let finished = ops.finished_runs.lock().await.clone();
        assert_eq!(1, finished.len());
        assert_eq!(ProjectReviewRunStatus::Failed, finished[0].status);
        assert_eq!(Some(ProjectReviewOutcome::Failed), finished[0].outcome);
        assert_eq!(Some(reviewer_id), finished[0].reviewer_agent_id);
        assert_eq!(
            Some("invalid input: failed to persist running review state".to_string()),
            finished[0].error
        );
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
                r#"{"outcome":"review_submitted","review_event":"request_changes","pr":726,"summary":"已提交 review","error":null}"#
                    .to_string(),
            ],
        );

        let result =
            run_project_review_once(&ops, project_id, CancellationToken::new(), request(726))
                .await
                .expect("review result");

        assert_eq!(ProjectReviewOutcome::ReviewSubmitted, result.outcome);
        assert_eq!(
            Some(ProjectReviewDecision::RequestChanges),
            result.review_event
        );
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
        assert_eq!(
            Some(ProjectReviewDecision::RequestChanges),
            finished[0].review_event
        );
        assert_eq!(Some(726), finished[0].pr);
    }

    #[tokio::test]
    async fn stale_target_overrides_model_result_and_still_cleans_reviewer() {
        let project_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let ops = FakeCycleOps::new(
            project_id,
            reviewer_id,
            vec![r#"{"outcome":"review_submitted","review_event":"approve","pr":726,"summary":"submitted","error":null}"#.to_string()],
        )
        .with_stale_target();

        let error =
            run_project_review_once(&ops, project_id, CancellationToken::new(), request(726))
                .await
                .expect_err("stale target must fail the run");

        assert!(
            error
                .to_string()
                .contains(super::super::REVIEW_TARGET_HEAD_CHANGED)
        );
        assert_eq!(vec![reviewer_id], *ops.deleted_agents.lock().await);
        let finished = ops.finished_runs.lock().await.clone();
        assert_eq!(ProjectReviewRunStatus::Failed, finished[0].status);
        assert_eq!(Some(726), finished[0].pr);
        assert!(
            finished[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains(super::super::REVIEW_TARGET_HEAD_CHANGED))
        );
    }

    #[tokio::test]
    async fn reviewer_report_for_another_pr_is_rejected_without_changing_run_target() {
        let project_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let ops = FakeCycleOps::new(
            project_id,
            reviewer_id,
            vec![r#"{"outcome":"review_submitted","review_event":"approve","pr":727,"summary":"submitted","error":null}"#.to_string()],
        );

        let error =
            run_project_review_once(&ops, project_id, CancellationToken::new(), request(726))
                .await
                .expect_err("a mismatched reviewer report must fail the run");

        assert!(error.to_string().contains("prepared target is PR #726"));
        assert_eq!(vec![reviewer_id], *ops.deleted_agents.lock().await);
        let finished = ops.finished_runs.lock().await.clone();
        assert_eq!(1, finished.len());
        assert_eq!(ProjectReviewRunStatus::Failed, finished[0].status);
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

    fn request(pr: u64) -> ProjectReviewRequest {
        ProjectReviewRequest {
            pr,
            head_sha_hint: Some("head-hint".to_string()),
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
            state: AgentState {
                resource: AgentResourceState::Ready,
                runtime: AgentRuntimeState {
                    last_turn: Some(AgentLastTurn {
                        turn_id: Uuid::new_v4(),
                        session_id: Uuid::new_v4(),
                        outcome: AgentTurnOutcomeKind::Completed,
                        reason: None,
                        usage: TokenUsage::default(),
                        finished_at: timestamp,
                    }),
                    ..AgentRuntimeState::default()
                },
                ..AgentState::default()
            },
            container_id: Some("container".to_string()),
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "mock".to_string(),
            provider_name: "Mock".to_string(),
            model: "mock-model".to_string(),
            reasoning_effort: Some("medium".to_string()),
            created_at: timestamp,
            updated_at: timestamp,
            token_usage: TokenUsage::default(),
        }
    }

    fn completed_wait_result() -> AgentWaitResult {
        let agent_id = pl_core::AgentId::new("reviewer").expect("agent id");
        let turn = pl_core::AgentTurnOutcome {
            turn_id: pl_core::TurnId::new("turn").expect("turn id"),
            session_id: pl_core::SessionId::new("session").expect("session id"),
            kind: TurnOutcomeKind::Completed,
            reason: None,
            failure: None,
            usage: pl_model::TokenUsage::default(),
            finished_at: 1,
        };
        AgentWaitResult {
            snapshot: pl_core::AgentSnapshot {
                identity: pl_core::AgentIdentity {
                    id: agent_id,
                    parent_id: None,
                    role: pl_core::AgentRoleId::new("reviewer").expect("role"),
                    depth: 0,
                },
                lifecycle: pl_core::AgentLifecycleState::Active,
                activity: pl_core::AgentActivityState::Idle,
                active_turn_id: None,
                active_session_id: None,
                pending_inputs: 0,
                last_turn: Some(turn.clone()),
                revision: 2,
                event_sequence: 1,
                updated_at: 1,
            },
            last_turn: Some(turn),
        }
    }
}
