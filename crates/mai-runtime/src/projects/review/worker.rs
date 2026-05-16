use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use chrono::{TimeDelta, Utc};
use futures::future::{AbortHandle, Abortable};
use mai_protocol::{
    AgentId, AgentSummary, GitProvider, ProjectCloneStatus, ProjectId, ProjectReviewOutcome,
    ProjectReviewRunStatus, ProjectReviewStatus, ProjectStatus, ProjectSummary, TurnId,
};
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

use super::ProjectReviewCycleResult;
use super::pool::PendingProjectReview;
use super::runs::FinishReviewRun;
use super::selector::ProjectReviewSelectorRunResult;
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

    fn project_git_provider(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<Option<GitProvider>>> + Send;

    fn run_project_review_selector(
        &self,
        project_id: ProjectId,
        cancellation_token: CancellationToken,
    ) -> impl Future<Output = Result<ProjectReviewSelectorRunResult>> + Send;

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
    project.review_pool.lock().await.clear();
    project.review_notify.notify_waiters();
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
        let error = err.to_string();
        if !super::project_review_error_is_retryable(&error) {
            let _ = ops
                .record_project_review_startup_failure(project_id, error.clone())
                .await;
        }
        let decision = super::project_review_loop_decision_for_error(error);
        let next = Utc::now() + TimeDelta::seconds(decision.delay.as_secs() as i64);
        let _ = ops
            .set_project_review_state(
                project_id,
                decision.status,
                ReviewStateUpdate {
                    next_review_at: Some(next),
                    outcome: decision.outcome,
                    summary_text: decision.summary,
                    error: decision.error,
                    ..Default::default()
                },
            )
            .await;
    }
    let mut startup_selector_ran = match ops.project(project_id).await {
        Ok(project) => project.review_pool.lock().await.has_pending(),
        Err(_) => false,
    };
    let mut next_poll_selector_at = Utc::now();
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

        let signal = match next_project_review_signal(&ops, project_id).await {
            Ok(Some(signal)) => signal,
            Ok(None) => {
                if !startup_selector_ran {
                    startup_selector_ran = true;
                    if let Err(err) = run_selector(&ops, project_id, &cancellation_token).await {
                        if matches!(err, RuntimeError::TurnCancelled)
                            && cancellation_token.is_cancelled()
                        {
                            break;
                        }
                        tracing::warn!(project_id = %project_id, "project review selector failed: {err}");
                    }
                    next_poll_selector_at = Utc::now()
                        + TimeDelta::seconds(super::PROJECT_REVIEW_IDLE_RETRY_SECS as i64);
                    continue;
                }
                if should_poll_selector(&ops, project_id, next_poll_selector_at).await {
                    next_poll_selector_at = Utc::now()
                        + TimeDelta::seconds(super::PROJECT_REVIEW_IDLE_RETRY_SECS as i64);
                    if let Err(err) = run_selector(&ops, project_id, &cancellation_token).await {
                        if matches!(err, RuntimeError::TurnCancelled)
                            && cancellation_token.is_cancelled()
                        {
                            break;
                        }
                        tracing::warn!(project_id = %project_id, "project review selector failed: {err}");
                    }
                    continue;
                }
                wait_for_project_review_signal(&ops, project_id, &cancellation_token).await;
                continue;
            }
            Err(err) => {
                tracing::warn!(project_id = %project_id, "failed to read project review pool: {err}");
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        let decision = ops
            .run_project_review_once(project_id, cancellation_token.clone(), Some(signal.pr))
            .await;
        let mut decision = match decision {
            Ok(result) => super::project_review_loop_decision_for_result(result),
            Err(RuntimeError::TurnCancelled) if cancellation_token.is_cancelled() => break,
            Err(err) => {
                let error = err.to_string();
                if super::project_review_error_is_retryable(&error) {
                    requeue_project_review_signal(&ops, project_id, signal).await;
                }
                super::project_review_loop_decision_for_error(error)
            }
        };
        if matches!(decision.outcome, Some(ProjectReviewOutcome::NoEligiblePr)) {
            decision.delay = Duration::ZERO;
            decision.status = ProjectReviewStatus::Idle;
        }
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

async fn run_selector(
    ops: &impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    cancellation_token: &CancellationToken,
) -> Result<()> {
    let _ = ops
        .set_project_review_state(
            project_id,
            ProjectReviewStatus::Selecting,
            ReviewStateUpdate::default(),
        )
        .await;
    let result = ops
        .run_project_review_selector(project_id, cancellation_token.clone())
        .await;
    let result = match result {
        Ok(result) => result,
        Err(err) => {
            let error = err.to_string();
            let decision = super::project_review_loop_decision_for_error(error);
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
            return Err(err);
        }
    };
    match result {
        ProjectReviewSelectorRunResult::Queued(_) => {
            let _ = ops
                .set_project_review_state(
                    project_id,
                    ProjectReviewStatus::Idle,
                    ReviewStateUpdate::default(),
                )
                .await;
        }
        ProjectReviewSelectorRunResult::NoEligiblePr => {
            let next_review_at =
                Some(Utc::now() + TimeDelta::seconds(super::PROJECT_REVIEW_IDLE_RETRY_SECS as i64));
            let _ = ops
                .set_project_review_state(
                    project_id,
                    ProjectReviewStatus::Waiting,
                    ReviewStateUpdate {
                        next_review_at,
                        outcome: Some(ProjectReviewOutcome::NoEligiblePr),
                        ..Default::default()
                    },
                )
                .await;
        }
    }
    Ok(())
}

async fn should_poll_selector(
    ops: &impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    next_poll_selector_at: chrono::DateTime<Utc>,
) -> bool {
    if Utc::now() < next_poll_selector_at {
        return false;
    }
    match ops.project_git_provider(project_id).await {
        Ok(Some(GitProvider::Github | GitProvider::GithubAppRelay)) | Ok(None) => true,
        Err(err) => {
            tracing::warn!(project_id = %project_id, "failed to read project git provider: {err}");
            true
        }
    }
}

async fn next_project_review_signal(
    ops: &impl ProjectReviewWorkerOps,
    project_id: ProjectId,
) -> Result<Option<PendingProjectReview>> {
    let project = ops.project(project_id).await?;
    Ok(project.review_pool.lock().await.next())
}

async fn requeue_project_review_signal(
    ops: &impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    signal: PendingProjectReview,
) {
    if let Ok(project) = ops.project(project_id).await {
        project.review_pool.lock().await.requeue(signal);
        project.review_notify.notify_waiters();
    }
}

async fn wait_for_project_review_signal(
    ops: &impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    cancellation_token: &CancellationToken,
) {
    let notify = match ops.project(project_id).await {
        Ok(project) => Arc::clone(&project.review_notify),
        Err(_) => return,
    };
    tokio::select! {
        _ = notify.notified() => {}
        _ = sleep(Duration::from_secs(1)) => {}
        _ = cancellation_token.cancelled() => {}
    }
}

fn project_ready_for_review(summary: &ProjectSummary) -> bool {
    summary.auto_review_enabled
        && summary.status == ProjectStatus::Ready
        && summary.clone_status == ProjectCloneStatus::Ready
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mai_protocol::{
        ProjectCloneStatus, ProjectReviewOutcome, ProjectReviewRunSummary, ProjectReviewStatus,
        ProjectStatus, ProjectSummary, now,
    };
    use pretty_assertions::assert_eq;
    use tokio::sync::{Mutex, Notify};
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use crate::state::ProjectRecord;

    use super::{
        FinishReviewRun, ProjectReviewCycleResult, ProjectReviewSelectorRunResult,
        ProjectReviewWorkerOps, ReviewStateUpdate, run_selector,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ReviewStateSnapshot {
        status: ProjectReviewStatus,
        next_review_at_set: bool,
        outcome: Option<ProjectReviewOutcome>,
        error: Option<String>,
    }

    #[derive(Clone)]
    struct FakeWorkerOps {
        project: Arc<ProjectRecord>,
        states: Arc<Mutex<Vec<ReviewStateSnapshot>>>,
        selector_started: Arc<Notify>,
        release_selector: Arc<Notify>,
    }

    impl FakeWorkerOps {
        fn new(project_id: Uuid) -> Self {
            Self {
                project: Arc::new(ProjectRecord::new(test_project_summary(project_id))),
                states: Arc::new(Mutex::new(Vec::new())),
                selector_started: Arc::new(Notify::new()),
                release_selector: Arc::new(Notify::new()),
            }
        }
    }

    impl ProjectReviewWorkerOps for FakeWorkerOps {
        async fn project(&self, _project_id: Uuid) -> crate::Result<Arc<ProjectRecord>> {
            Ok(Arc::clone(&self.project))
        }

        async fn project_ids(&self) -> Vec<Uuid> {
            vec![]
        }

        async fn project_auto_reviewer_agents(
            &self,
            _project_id: Uuid,
        ) -> Vec<mai_protocol::AgentSummary> {
            vec![]
        }

        async fn load_project_review_runs(
            &self,
            _project_id: Uuid,
            _offset: usize,
            _limit: usize,
        ) -> crate::Result<Vec<ProjectReviewRunSummary>> {
            Ok(vec![])
        }

        async fn finish_project_review_run(&self, _request: FinishReviewRun) -> crate::Result<()> {
            Ok(())
        }

        async fn cancel_active_project_review_runs(
            &self,
            _project_id: Uuid,
            _reviewer_agent_id: Option<Uuid>,
            _run_list_limit: usize,
        ) -> crate::Result<()> {
            Ok(())
        }

        async fn record_project_review_startup_failure(
            &self,
            _project_id: Uuid,
            _error: String,
        ) -> crate::Result<()> {
            Ok(())
        }

        async fn set_project_review_state(
            &self,
            _project_id: Uuid,
            status: ProjectReviewStatus,
            update: ReviewStateUpdate,
        ) -> crate::Result<ProjectSummary> {
            self.states.lock().await.push(ReviewStateSnapshot {
                status: status.clone(),
                next_review_at_set: update.next_review_at.is_some(),
                outcome: update.outcome.clone(),
                error: update.error.clone(),
            });
            let mut summary = self.project.summary.write().await;
            summary.review_status = status;
            summary.next_review_at = update.next_review_at;
            if let Some(outcome) = update.outcome {
                summary.last_review_outcome = Some(outcome);
            }
            summary.review_last_error = update.error;
            Ok(summary.clone())
        }

        async fn ensure_project_review_workspace(&self, _project_id: Uuid) -> crate::Result<()> {
            Ok(())
        }

        async fn project_git_provider(
            &self,
            _project_id: Uuid,
        ) -> crate::Result<Option<mai_protocol::GitProvider>> {
            Ok(Some(mai_protocol::GitProvider::Github))
        }

        async fn run_project_review_selector(
            &self,
            _project_id: Uuid,
            _cancellation_token: CancellationToken,
        ) -> crate::Result<ProjectReviewSelectorRunResult> {
            self.selector_started.notify_waiters();
            self.release_selector.notified().await;
            Ok(ProjectReviewSelectorRunResult::NoEligiblePr)
        }

        async fn run_project_review_once(
            &self,
            _project_id: Uuid,
            _cancellation_token: CancellationToken,
            _target_pr: Option<u64>,
        ) -> crate::Result<ProjectReviewCycleResult> {
            panic!("selector state test must not run reviewer cycle");
        }

        async fn agent_current_turn(&self, _agent_id: Uuid) -> crate::Result<Option<Uuid>> {
            Ok(None)
        }

        async fn cancel_agent_turn(&self, _agent_id: Uuid, _turn_id: Uuid) -> crate::Result<()> {
            Ok(())
        }

        async fn delete_agent(&self, _agent_id: Uuid) -> crate::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn selector_status_is_visible_while_selecting_and_waiting_when_empty() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        let task_ops = ops.clone();
        let selector_task = tokio::spawn(async move {
            run_selector(&task_ops, project_id, &CancellationToken::new())
                .await
                .expect("run selector")
        });

        ops.selector_started.notified().await;
        assert_eq!(
            Some(ProjectReviewStatus::Selecting),
            ops.states
                .lock()
                .await
                .last()
                .map(|state| state.status.clone())
        );

        ops.release_selector.notify_waiters();
        selector_task.await.expect("selector task");

        let states = ops.states.lock().await.clone();
        assert_eq!(
            vec![
                ReviewStateSnapshot {
                    status: ProjectReviewStatus::Selecting,
                    next_review_at_set: false,
                    outcome: None,
                    error: None,
                },
                ReviewStateSnapshot {
                    status: ProjectReviewStatus::Waiting,
                    next_review_at_set: true,
                    outcome: Some(ProjectReviewOutcome::NoEligiblePr),
                    error: None,
                },
            ],
            states
        );
    }

    fn test_project_summary(project_id: Uuid) -> ProjectSummary {
        ProjectSummary {
            id: project_id,
            name: "owner/repo".to_string(),
            status: ProjectStatus::Ready,
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_full_name: "owner/repo".to_string(),
            git_account_id: Some("account-1".to_string()),
            repository_id: 42,
            installation_id: 0,
            installation_account: "owner".to_string(),
            branch: "main".to_string(),
            docker_image: "unused".to_string(),
            clone_status: ProjectCloneStatus::Ready,
            maintainer_agent_id: Uuid::new_v4(),
            created_at: now(),
            updated_at: now(),
            last_error: None,
            auto_review_enabled: true,
            reviewer_extra_prompt: None,
            review_status: ProjectReviewStatus::Idle,
            current_reviewer_agent_id: None,
            last_review_started_at: None,
            last_review_finished_at: None,
            next_review_at: None,
            last_review_outcome: None,
            review_last_error: None,
        }
    }
}
