use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use chrono::{TimeDelta, Utc};
use futures::future::{AbortHandle, Abortable};
use mai_protocol::{
    AgentId, AgentSummary, ProjectCloneStatus, ProjectId, ProjectReviewRunStatus,
    ProjectReviewStatus, ProjectStatus, ProjectSummary, TurnId,
};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use super::ProjectReviewCycleResult;
use super::runs::FinishReviewRun;
use super::state::ReviewStateUpdate;
use crate::state::{ProjectRecord, ProjectReviewWorker};
use crate::{Result, RuntimeError};

/// Provides project review worker lifecycle side effects while keeping the
/// background loop independent of the full runtime facade.
pub(crate) trait ProjectReviewWorkerOps: Clone + Send + Sync + 'static {
    fn project(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<Arc<ProjectRecord>>> + Send;

    fn project_ids(&self) -> impl Future<Output = Vec<ProjectId>> + Send;

    fn project_auto_reviewer_agents(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Vec<AgentSummary>> + Send;

    fn load_project_review_runs(
        &self,
        project_id: ProjectId,
        offset: usize,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<mai_protocol::ProjectReviewRunSummary>>> + Send;

    fn finish_project_review_run(
        &self,
        request: FinishReviewRun,
    ) -> impl Future<Output = Result<()>> + Send;

    fn cancel_active_project_review_runs(
        &self,
        project_id: ProjectId,
        reviewer_agent_id: Option<AgentId>,
        run_list_limit: usize,
    ) -> impl Future<Output = Result<()>> + Send;

    fn record_project_review_startup_failure(
        &self,
        project_id: ProjectId,
        error: String,
    ) -> impl Future<Output = Result<()>> + Send;

    fn set_project_review_state(
        &self,
        project_id: ProjectId,
        status: ProjectReviewStatus,
        update: ReviewStateUpdate,
    ) -> impl Future<Output = Result<ProjectSummary>> + Send;

    fn ensure_project_review_workspace(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<()>> + Send;

    fn run_project_review_once(
        &self,
        project_id: ProjectId,
        cancellation_token: CancellationToken,
        target_pr: Option<u64>,
    ) -> impl Future<Output = Result<ProjectReviewCycleResult>> + Send;

    fn agent_current_turn(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<Option<TurnId>>> + Send;

    fn cancel_agent_turn(
        &self,
        agent_id: AgentId,
        turn_id: TurnId,
    ) -> impl Future<Output = Result<()>> + Send;

    fn delete_agent(&self, agent_id: AgentId) -> impl Future<Output = Result<()>> + Send;
}

pub(crate) async fn start_enabled_project_review_workers(ops: impl ProjectReviewWorkerOps) {
    for project_id in ops.project_ids().await {
        if let Err(err) = start_project_review_loop_if_ready(ops.clone(), project_id).await {
            tracing::warn!(project_id = %project_id, "failed to start project review loop: {err}");
        }
    }
}

pub(crate) async fn reconcile_project_review_singletons(
    ops: impl ProjectReviewWorkerOps,
    run_list_limit: usize,
) {
    for project_id in ops.project_ids().await {
        if let Err(err) =
            reconcile_project_review_singleton(ops.clone(), project_id, run_list_limit).await
        {
            tracing::warn!(project_id = %project_id, "failed to reconcile project reviewer singleton: {err}");
        }
    }
}

pub(crate) async fn reconcile_project_review_singleton(
    ops: impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    run_list_limit: usize,
) -> Result<()> {
    let project = ops.project(project_id).await?;
    let summary = project.summary.read().await.clone();
    let mut stale_reviewer_ids = HashSet::new();
    if let Some(reviewer_id) = summary.current_reviewer_agent_id {
        stale_reviewer_ids.insert(reviewer_id);
    }

    let runs = ops
        .load_project_review_runs(project_id, 0, run_list_limit)
        .await?;
    let mut has_stale_activity = summary.current_reviewer_agent_id.is_some();
    for run in runs {
        if run.finished_at.is_some()
            || !matches!(
                run.status,
                ProjectReviewRunStatus::Syncing | ProjectReviewRunStatus::Running
            )
        {
            continue;
        }
        has_stale_activity = true;
        if let Some(reviewer_id) = run.reviewer_agent_id {
            stale_reviewer_ids.insert(reviewer_id);
        }
        let _ = ops
            .finish_project_review_run(FinishReviewRun {
                run_id: run.id,
                project_id,
                reviewer_agent_id: run.reviewer_agent_id,
                turn_id: run.turn_id,
                status: ProjectReviewRunStatus::Cancelled,
                outcome: None,
                pr: run.pr,
                summary_text: run.summary,
                error: Some("review interrupted by server restart".to_string()),
            })
            .await;
    }

    for agent in ops.project_auto_reviewer_agents(project_id).await {
        has_stale_activity = true;
        stale_reviewer_ids.insert(agent.id);
    }

    for reviewer_id in stale_reviewer_ids {
        if let Err(err) = ops.delete_agent(reviewer_id).await {
            tracing::warn!(
                project_id = %project_id,
                reviewer_id = %reviewer_id,
                "failed to delete stale project reviewer agent: {err}"
            );
        }
    }

    if has_stale_activity {
        let status = if summary.auto_review_enabled {
            ProjectReviewStatus::Idle
        } else {
            ProjectReviewStatus::Disabled
        };
        let _ = ops
            .set_project_review_state(project_id, status, ReviewStateUpdate::default())
            .await?;
    }
    Ok(())
}

pub(crate) async fn start_project_review_loop_if_ready(
    ops: impl ProjectReviewWorkerOps,
    project_id: ProjectId,
) -> Result<()> {
    let project = ops.project(project_id).await?;
    let should_start = {
        let summary = project.summary.read().await;
        project_ready_for_review(&summary)
    };
    if !should_start {
        return Ok(());
    }

    let mut worker = project.review_worker.lock().await;
    if worker.is_some() {
        return Ok(());
    }
    let cancellation_token = CancellationToken::new();
    let token = cancellation_token.clone();
    let loop_ops = ops.clone();
    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    tokio::spawn(Abortable::new(
        async move {
            run_project_review_loop(loop_ops, project_id, token).await;
        },
        abort_registration,
    ));
    *worker = Some(ProjectReviewWorker {
        cancellation_token,
        abort_handle,
    });
    Ok(())
}

pub(crate) async fn stop_project_review_loop(
    ops: impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    run_list_limit: usize,
) {
    let project = match ops.project(project_id).await {
        Ok(project) => project,
        Err(_) => return,
    };
    let worker = project.review_worker.lock().await.take();
    if let Some(worker) = worker {
        worker.cancellation_token.cancel();
        worker.abort_handle.abort();
    }
    let reviewer_id = project.summary.read().await.current_reviewer_agent_id;
    let _ = ops
        .cancel_active_project_review_runs(project_id, reviewer_id, run_list_limit)
        .await;
    if let Some(reviewer_id) = reviewer_id {
        if let Ok(Some(turn_id)) = ops.agent_current_turn(reviewer_id).await {
            let _ = ops.cancel_agent_turn(reviewer_id, turn_id).await;
        }
        let _ = ops.delete_agent(reviewer_id).await;
    }
    let _ = ops
        .set_project_review_state(
            project_id,
            ProjectReviewStatus::Disabled,
            ReviewStateUpdate {
                force_disabled: true,
                ..Default::default()
            },
        )
        .await;
}

pub(crate) async fn run_project_review_loop(
    ops: impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    cancellation_token: CancellationToken,
) {
    if let Err(err) = ops.ensure_project_review_workspace(project_id).await {
        let _ = ops
            .record_project_review_startup_failure(project_id, err.to_string())
            .await;
        let next = Utc::now() + TimeDelta::seconds(super::PROJECT_REVIEW_FAILURE_RETRY_SECS as i64);
        let _ = ops
            .set_project_review_state(
                project_id,
                ProjectReviewStatus::Failed,
                ReviewStateUpdate {
                    next_review_at: Some(next),
                    error: Some(err.to_string()),
                    ..Default::default()
                },
            )
            .await;
    }
    loop {
        if cancellation_token.is_cancelled() {
            break;
        }
        let should_continue = match ops.project(project_id).await {
            Ok(project) => {
                let summary = project.summary.read().await;
                project_ready_for_review(&summary)
            }
            Err(_) => false,
        };
        if !should_continue {
            break;
        }

        let decision = ops
            .run_project_review_once(project_id, cancellation_token.clone(), None)
            .await;
        let decision = match decision {
            Ok(result) => super::project_review_loop_decision_for_result(result),
            Err(RuntimeError::TurnCancelled) if cancellation_token.is_cancelled() => break,
            Err(err) => super::project_review_loop_decision_for_error(err.to_string()),
        };
        let next_review_at = (decision.delay.as_secs() > 0)
            .then(|| Utc::now() + TimeDelta::seconds(decision.delay.as_secs() as i64));
        let _ = ops
            .set_project_review_state(
                project_id,
                decision.status,
                ReviewStateUpdate {
                    next_review_at,
                    outcome: decision.outcome,
                    summary_text: decision.summary,
                    error: decision.error,
                    ..Default::default()
                },
            )
            .await;
        if decision.delay.is_zero() {
            continue;
        }
        tokio::select! {
            _ = sleep(decision.delay) => {}
            _ = cancellation_token.cancelled() => break,
        }
    }
    if let Ok(project) = ops.project(project_id).await {
        let mut worker = project.review_worker.lock().await;
        *worker = None;
    }
}

fn project_ready_for_review(summary: &ProjectSummary) -> bool {
    summary.auto_review_enabled
        && summary.status == ProjectStatus::Ready
        && summary.clone_status == ProjectCloneStatus::Ready
}
