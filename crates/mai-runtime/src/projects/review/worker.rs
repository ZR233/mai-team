use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use futures::future::{AbortHandle, Abortable};
use mai_protocol::{
    AgentId, AgentResourceState, AgentRuntimeLifecycle, AgentSummary, GitProvider,
    ProjectCloneStatus, ProjectId, ProjectReviewJobSummary, ProjectReviewOutcome,
    ProjectReviewRunStatus, ProjectReviewStatus, ProjectStatus, ProjectSummary, TurnId,
};
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

use super::ProjectReviewCycleResult;
#[cfg(test)]
use super::eligibility::SelectedProjectReviewPr;
use super::project_review_retry_backoff;
#[cfg(test)]
use super::runs::FinishReviewRun;
use super::selector::ProjectReviewSelectorRunResult;
#[cfg(test)]
use super::singleton::{ProjectReviewRepairReason, repair_project_review_singleton};
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

    #[cfg(test)]
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

    fn ensure_project_repository_ready(
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

    #[cfg(test)]
    fn select_project_review_pr(
        &self,
        project_id: ProjectId,
        pr: u64,
        head_sha_hint: Option<String>,
    ) -> impl Future<Output = Result<Option<SelectedProjectReviewPr>>> + Send;

    #[cfg(test)]
    fn enqueue_project_review_signals(
        &self,
        project_id: ProjectId,
        signals: Vec<crate::projects::review::pool::ProjectReviewSignalInput>,
    ) -> impl Future<Output = Result<crate::ProjectReviewQueueSummary>> + Send;

    #[cfg(test)]
    fn run_project_review_once(
        &self,
        project_id: ProjectId,
        cancellation_token: CancellationToken,
        request: super::target::ProjectReviewRequest,
    ) -> impl Future<Output = Result<ProjectReviewCycleResult>> + Send;

    fn project_has_active_review_jobs(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<bool>> + Send;

    fn claim_due_project_review_job(
        &self,
        project_id: ProjectId,
        owner: String,
        now: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> impl Future<Output = Result<Option<ProjectReviewJobSummary>>> + Send;

    fn load_project_review_job(
        &self,
        project_id: ProjectId,
        job_id: uuid::Uuid,
    ) -> impl Future<Output = Result<Option<ProjectReviewJobSummary>>> + Send;

    fn load_active_project_review_job(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<Option<ProjectReviewJobSummary>>> + Send;

    fn save_claimed_project_review_job(
        &self,
        job: ProjectReviewJobSummary,
        owner: String,
    ) -> impl Future<Output = Result<bool>> + Send;

    fn heartbeat_project_review_job(
        &self,
        job_id: uuid::Uuid,
        owner: String,
        updated_at: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> impl Future<Output = Result<bool>> + Send;

    fn recover_expired_project_review_jobs(
        &self,
        now: DateTime<Utc>,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn cancel_active_project_review_jobs(
        &self,
        project_id: ProjectId,
        now: DateTime<Utc>,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn run_project_review_job_attempt(
        &self,
        job: ProjectReviewJobSummary,
        owner: String,
        cancellation_token: CancellationToken,
    ) -> impl Future<Output = Result<ProjectReviewCycleResult>> + Send;

    fn reconcile_project_review_job(
        &self,
        job: ProjectReviewJobSummary,
    ) -> impl Future<Output = Result<Option<mai_protocol::ProjectReviewSubmissionReceipt>>> + Send;

    fn agent_current_turn(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<Option<TurnId>>> + Send;

    fn cancel_agent_turn(
        &self,
        agent_id: AgentId,
        turn_id: TurnId,
    ) -> impl Future<Output = Result<()>> + Send;

    fn find_project_review_job_reviewer(
        &self,
        job: ProjectReviewJobSummary,
    ) -> impl Future<Output = Result<Option<AgentId>>> + Send;

    fn delete_agent(&self, agent_id: AgentId) -> impl Future<Output = Result<()>> + Send;
}

pub(crate) async fn start_enabled_project_review_workers(ops: impl ProjectReviewWorkerOps) {
    if let Err(error) = ops.recover_expired_project_review_jobs(Utc::now()).await {
        tracing::warn!("failed to recover expired project review jobs: {error}");
    }
    for project_id in ops.project_ids().await {
        if let Err(err) = start_project_review_loop_if_ready(ops.clone(), project_id).await {
            tracing::warn!(project_id = %project_id, "failed to start project review loop: {err}");
        }
    }
}

pub(crate) async fn start_project_review_loop_if_ready(
    ops: impl ProjectReviewWorkerOps,
    project_id: ProjectId,
) -> Result<()> {
    let project = ops.project(project_id).await?;
    let (resources_ready, auto_review_enabled) = {
        let summary = project.summary.read().await;
        (
            project_resources_ready_for_review(&summary),
            summary.auto_review_enabled,
        )
    };
    let has_pending_manual_review = ops.project_has_active_review_jobs(project_id).await?;
    if !resources_ready || (!auto_review_enabled && !has_pending_manual_review) {
        return Ok(());
    }

    let git_provider = if auto_review_enabled {
        match ops.project_git_provider(project_id).await {
            Ok(provider) => provider,
            Err(err) => {
                tracing::warn!(project_id = %project_id, "failed to read project git provider: {err}");
                None
            }
        }
    } else {
        None
    };
    let mut worker = project.review_worker.lock().await;
    if let Some(worker) = worker.as_mut() {
        if auto_review_enabled {
            let context = ProjectReviewTaskContext {
                ops: ops.clone(),
                project_id,
                cancellation_token: worker.cancellation_token.clone(),
            };
            #[cfg(test)]
            if worker.relay_selector_abort_handle.is_none() {
                worker.relay_selector_abort_handle = Some(spawn_project_review_child(
                    super::relay_selector::run_project_review_relay_selector_loop(
                        context.ops.clone(),
                        context.project_id,
                        context.cancellation_token.clone(),
                    ),
                ));
            }
            if worker.selector_abort_handle.is_none()
                && matches!(
                    git_provider,
                    Some(GitProvider::Github) | Some(GitProvider::GithubAppRelay)
                )
            {
                worker.selector_abort_handle = Some(spawn_project_review_child(
                    run_project_review_selector_loop(context),
                ));
            }
        }
        return Ok(());
    }
    let cancellation_token = CancellationToken::new();
    let context = ProjectReviewTaskContext {
        ops: ops.clone(),
        project_id,
        cancellation_token: cancellation_token.clone(),
    };
    let pool_abort_handle =
        spawn_project_review_child(run_project_review_pool_worker(context.clone()));
    #[cfg(test)]
    let relay_selector_abort_handle = auto_review_enabled.then(|| {
        spawn_project_review_child(
            super::relay_selector::run_project_review_relay_selector_loop(
                context.ops.clone(),
                context.project_id,
                context.cancellation_token.clone(),
            ),
        )
    });
    let selector_abort_handle = match (auto_review_enabled, git_provider) {
        (true, Some(GitProvider::Github) | Some(GitProvider::GithubAppRelay)) => Some(
            spawn_project_review_child(run_project_review_selector_loop(context.clone())),
        ),
        (true, None)
        | (false, Some(GitProvider::Github))
        | (false, Some(GitProvider::GithubAppRelay))
        | (false, None) => None,
    };
    *worker = Some(ProjectReviewWorker {
        cancellation_token,
        pool_abort_handle,
        selector_abort_handle,
        #[cfg(test)]
        relay_selector_abort_handle,
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
        worker.pool_abort_handle.abort();
        if let Some(selector_abort_handle) = worker.selector_abort_handle {
            selector_abort_handle.abort();
        }
        #[cfg(test)]
        if let Some(relay_selector_abort_handle) = worker.relay_selector_abort_handle {
            relay_selector_abort_handle.abort();
        }
    }
    let _ = ops
        .cancel_active_project_review_jobs(project_id, Utc::now())
        .await;
    project.review_pool.lock().await.clear();
    project.review_notify.notify_waiters();
    #[cfg(test)]
    {
        project.relay_review_queue.lock().await.clear();
        project.relay_review_notify.notify_waiters();
    }
    let mut reviewer_ids = active_project_reviewer_ids(&ops, project_id, run_list_limit).await;
    let reviewer_id = project.summary.read().await.current_reviewer_agent_id;
    if let Some(reviewer_id) = reviewer_id {
        reviewer_ids.insert(reviewer_id);
    }
    let _ = ops
        .cancel_active_project_review_runs(project_id, None, run_list_limit)
        .await;
    for reviewer_id in reviewer_ids {
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

#[cfg(test)]
pub(crate) async fn run_project_review_loop(
    ops: impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    cancellation_token: CancellationToken,
) {
    let context = ProjectReviewTaskContext {
        ops,
        project_id,
        cancellation_token,
    };
    run_project_review_pool_worker(context).await;
}

#[cfg(test)]
pub(crate) async fn run_project_review_relay_selector_loop_for_test(
    ops: impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    cancellation_token: CancellationToken,
) {
    super::relay_selector::run_project_review_relay_selector_loop(
        ops,
        project_id,
        cancellation_token,
    )
    .await;
}

#[derive(Clone)]
pub(super) struct ProjectReviewTaskContext<Ops> {
    pub(super) ops: Ops,
    pub(super) project_id: ProjectId,
    pub(super) cancellation_token: CancellationToken,
}

fn spawn_project_review_child(future: impl Future<Output = ()> + Send + 'static) -> AbortHandle {
    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    tokio::spawn(Abortable::new(future, abort_registration));
    abort_handle
}

async fn run_project_review_pool_worker(
    ops: ProjectReviewTaskContext<impl ProjectReviewWorkerOps>,
) {
    let mut workspace_backoff = project_review_retry_backoff();
    while !ops.cancellation_token.is_cancelled() {
        if !project_still_ready(&ops).await {
            break;
        }
        match ops
            .ops
            .ensure_project_repository_ready(ops.project_id)
            .await
        {
            Ok(()) => break,
            Err(err) => {
                let error = err.to_string();
                let retryable = super::project_review_error_is_retryable(&error);
                if !retryable {
                    let _ = ops
                        .ops
                        .record_project_review_startup_failure(ops.project_id, error.clone())
                        .await;
                }
                let decision = super::project_review_loop_decision_for_error(error);
                let delay = if retryable {
                    workspace_backoff.next_delay()
                } else {
                    workspace_backoff.reset();
                    decision.delay
                };
                let next = Utc::now() + TimeDelta::seconds(delay.as_secs() as i64);
                let _ = ops
                    .ops
                    .set_project_review_state(
                        ops.project_id,
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
                if !wait_for_project_review_startup_retry(
                    &ops.ops,
                    ops.project_id,
                    &ops.cancellation_token,
                    delay,
                )
                .await
                {
                    break;
                }
            }
        }
    }
    let owner = format!("mai-review:{}:{}", std::process::id(), uuid::Uuid::new_v4());
    loop {
        if ops.cancellation_token.is_cancelled() {
            break;
        }
        if !project_still_ready(&ops).await {
            break;
        }

        let claimed_at = Utc::now();
        if let Err(err) = ops
            .ops
            .recover_expired_project_review_jobs(claimed_at)
            .await
        {
            tracing::warn!(project_id = %ops.project_id, "failed to recover expired project review leases: {err}");
            if !wait_or_cancel(&ops.cancellation_token, Duration::from_secs(1)).await {
                break;
            }
            continue;
        }
        let job = match ops
            .ops
            .claim_due_project_review_job(
                ops.project_id,
                owner.clone(),
                claimed_at,
                claimed_at + TimeDelta::seconds(super::job::REVIEW_JOB_LEASE_SECONDS),
            )
            .await
        {
            Ok(Some(job)) => job,
            Ok(None) => {
                wait_for_project_review_signal(&ops.ops, ops.project_id, &ops.cancellation_token)
                    .await;
                continue;
            }
            Err(err) => {
                tracing::warn!(project_id = %ops.project_id, "failed to claim project review job: {err}");
                if !wait_or_cancel(&ops.cancellation_token, Duration::from_secs(1)).await {
                    break;
                }
                continue;
            }
        };
        if !super::job_worker::run_claimed_project_review_job(&ops, job, owner.clone()).await {
            break;
        }
    }
    ops.cancellation_token.cancel();
    if let Ok(project) = ops.ops.project(ops.project_id).await {
        let mut worker = project.review_worker.lock().await;
        *worker = None;
    }
}

async fn run_project_review_selector_loop(
    ops: ProjectReviewTaskContext<impl ProjectReviewWorkerOps>,
) {
    run_project_review_selector_loop_with_interval(
        ops,
        Duration::from_secs(super::PROJECT_REVIEW_SELECTOR_INTERVAL_SECS),
    )
    .await;
}

async fn run_project_review_selector_loop_with_interval(
    ops: ProjectReviewTaskContext<impl ProjectReviewWorkerOps>,
    scan_interval: Duration,
) {
    let mut selector_backoff = project_review_retry_backoff();
    loop {
        if ops.cancellation_token.is_cancelled() || !project_still_ready(&ops).await {
            break;
        }

        let retry_delay = selector_backoff.next_delay();
        let schedule = ProjectReviewSelectorAttemptSchedule {
            normal_delay: scan_interval,
            retry_delay,
        };
        match run_selector_attempt(&ops, schedule).await {
            Ok(_) => {
                selector_backoff.reset();
                if !wait_or_cancel(&ops.cancellation_token, scan_interval).await {
                    break;
                }
            }
            Err(RuntimeError::TurnCancelled) if ops.cancellation_token.is_cancelled() => break,
            Err(err) => {
                tracing::warn!(project_id = %ops.project_id, "project review selector failed: {err}");
                if !wait_or_cancel(&ops.cancellation_token, retry_delay).await {
                    break;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ProjectReviewSelectorAttemptSchedule {
    normal_delay: Duration,
    retry_delay: Duration,
}

impl From<Duration> for ProjectReviewSelectorAttemptSchedule {
    fn from(delay: Duration) -> Self {
        Self {
            normal_delay: delay,
            retry_delay: delay,
        }
    }
}

async fn run_selector_attempt(
    ops: &ProjectReviewTaskContext<impl ProjectReviewWorkerOps>,
    schedule: impl Into<ProjectReviewSelectorAttemptSchedule>,
) -> Result<ProjectReviewSelectorRunResult> {
    let schedule = schedule.into();
    set_selector_state_if_visible(
        ops,
        ProjectReviewStatus::Selecting,
        ReviewStateUpdate::default(),
    )
    .await;
    let result = ops
        .ops
        .run_project_review_selector(ops.project_id, ops.cancellation_token.clone())
        .await;
    match result {
        Ok(result) => {
            match &result {
                ProjectReviewSelectorRunResult::Queued { .. } => {
                    set_selector_state_if_visible(
                        ops,
                        ProjectReviewStatus::Idle,
                        ReviewStateUpdate::default(),
                    )
                    .await;
                }
                ProjectReviewSelectorRunResult::NoEligiblePr => {
                    set_selector_state_if_visible(
                        ops,
                        ProjectReviewStatus::Waiting,
                        ReviewStateUpdate {
                            next_review_at: next_review_at_after(schedule.normal_delay),
                            outcome: Some(ProjectReviewOutcome::NoEligiblePr),
                            ..Default::default()
                        },
                    )
                    .await;
                }
            }
            Ok(result)
        }
        Err(err) => {
            if !(matches!(err, RuntimeError::TurnCancelled)
                && ops.cancellation_token.is_cancelled())
            {
                set_selector_state_if_visible(
                    ops,
                    ProjectReviewStatus::Waiting,
                    ReviewStateUpdate {
                        next_review_at: next_review_at_after(schedule.retry_delay),
                        error: Some(err.to_string()),
                        ..Default::default()
                    },
                )
                .await;
            }
            Err(err)
        }
    }
}

fn next_review_at_after(delay: Duration) -> Option<DateTime<Utc>> {
    (!delay.is_zero()).then(|| Utc::now() + TimeDelta::seconds(delay.as_secs() as i64))
}

async fn set_selector_state_if_visible(
    ops: &ProjectReviewTaskContext<impl ProjectReviewWorkerOps>,
    status: ProjectReviewStatus,
    update: ReviewStateUpdate,
) {
    if selector_state_visible(ops).await {
        let _ = ops
            .ops
            .set_project_review_state(ops.project_id, status, update)
            .await;
    }
}

async fn selector_state_visible(
    ops: &ProjectReviewTaskContext<impl ProjectReviewWorkerOps>,
) -> bool {
    if ops
        .ops
        .project_has_active_review_jobs(ops.project_id)
        .await
        .unwrap_or(true)
    {
        return false;
    }
    let Ok(project) = ops.ops.project(ops.project_id).await else {
        return false;
    };
    let summary = project.summary.read().await;
    summary.current_reviewer_agent_id.is_none()
        && !matches!(
            summary.review_status,
            ProjectReviewStatus::Syncing | ProjectReviewStatus::Running
        )
}

async fn project_still_ready(ops: &ProjectReviewTaskContext<impl ProjectReviewWorkerOps>) -> bool {
    match ops.ops.project(ops.project_id).await {
        Ok(project) => {
            let summary = project.summary.read().await;
            project_resources_ready_for_review(&summary)
        }
        Err(_) => false,
    }
}

async fn wait_or_cancel(cancellation_token: &CancellationToken, delay: Duration) -> bool {
    if delay.is_zero() {
        return true;
    }
    tokio::select! {
        _ = sleep(delay) => true,
        _ = cancellation_token.cancelled() => false,
    }
}

async fn active_project_reviewer_ids(
    ops: &impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    run_list_limit: usize,
) -> HashSet<AgentId> {
    let mut reviewer_ids = HashSet::new();
    if let Ok(runs) = ops
        .load_project_review_runs(project_id, 0, run_list_limit)
        .await
    {
        for run in runs {
            if run.finished_at.is_some()
                || !matches!(
                    run.status,
                    ProjectReviewRunStatus::Syncing | ProjectReviewRunStatus::Running
                )
            {
                continue;
            }
            if let Some(reviewer_id) = run.reviewer_agent_id {
                reviewer_ids.insert(reviewer_id);
            }
        }
    }
    for agent in ops.project_auto_reviewer_agents(project_id).await {
        if project_reviewer_agent_is_active(&agent) {
            reviewer_ids.insert(agent.id);
        }
    }
    reviewer_ids
}

fn project_reviewer_agent_is_active(agent: &AgentSummary) -> bool {
    matches!(
        agent.state.resource,
        AgentResourceState::Provisioning | AgentResourceState::Ready
    ) && agent.state.runtime.lifecycle == AgentRuntimeLifecycle::Active
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

async fn wait_for_project_review_startup_retry(
    ops: &impl ProjectReviewWorkerOps,
    project_id: ProjectId,
    cancellation_token: &CancellationToken,
    delay: Duration,
) -> bool {
    if delay.is_zero() {
        return true;
    }
    let notify = match ops.project(project_id).await {
        Ok(project) => Arc::clone(&project.review_notify),
        Err(_) => return true,
    };
    tokio::select! {
        _ = sleep(delay) => true,
        _ = notify.notified() => true,
        _ = cancellation_token.cancelled() => false,
    }
}

fn project_resources_ready_for_review(summary: &ProjectSummary) -> bool {
    summary.status == ProjectStatus::Ready && summary.clone_status == ProjectCloneStatus::Ready
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mai_protocol::{
        AgentResourceState, AgentRole, AgentRuntimeActivity, AgentRuntimeState, AgentState,
        ProjectCloneStatus, ProjectReviewDecision, ProjectReviewFailureCategory,
        ProjectReviewJobSource, ProjectReviewJobStatus, ProjectReviewJobSummary,
        ProjectReviewOutcome, ProjectReviewRunStatus, ProjectReviewRunSummary, ProjectReviewStatus,
        ProjectStatus, ProjectSummary, TokenUsage, now,
    };
    use pretty_assertions::assert_eq;
    use tokio::sync::{Mutex, Notify};
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use crate::projects::review::eligibility::SelectedProjectReviewPr;
    use crate::projects::review::pool::ProjectReviewSignalInput;
    use crate::projects::review::relay_queue::ProjectReviewRelaySignalInput;
    use crate::state::ProjectRecord;

    use super::{
        FinishReviewRun, ProjectReviewCycleResult, ProjectReviewRepairReason,
        ProjectReviewSelectorRunResult, ProjectReviewTaskContext, ProjectReviewWorkerOps,
        ReviewStateUpdate, repair_project_review_singleton, run_selector_attempt,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ReviewStateSnapshot {
        status: ProjectReviewStatus,
        next_review_at_set: bool,
        next_review_after_secs: Option<i64>,
        outcome: Option<ProjectReviewOutcome>,
        error: Option<String>,
    }

    #[derive(Clone)]
    enum FakeSelectorBehavior {
        NoEligiblePr,
        Queued,
        Error(&'static str),
    }

    #[derive(Clone)]
    struct FakeWorkerOps {
        project: Arc<ProjectRecord>,
        states: Arc<Mutex<Vec<ReviewStateSnapshot>>>,
        selector_started: Arc<Notify>,
        release_selector: Arc<Notify>,
        selector_calls: Arc<Mutex<u64>>,
        selector_behaviors: Arc<Mutex<Vec<FakeSelectorBehavior>>>,
        relay_selection_calls: Arc<Mutex<Vec<u64>>>,
        failed_relay_prs: Arc<Mutex<Vec<u64>>>,
        ineligible_relay_prs: Arc<Mutex<Vec<u64>>>,
        failed_enqueue_prs: Arc<Mutex<Vec<u64>>>,
        cache_ready_errors: Arc<Mutex<Vec<String>>>,
        reviewed_prs: Arc<Mutex<Vec<Option<u64>>>>,
        review_errors: Arc<Mutex<Vec<String>>>,
        auto_reviewers: Arc<Mutex<Vec<mai_protocol::AgentSummary>>>,
        review_runs: Arc<Mutex<Vec<ProjectReviewRunSummary>>>,
        review_jobs: Arc<Mutex<Vec<ProjectReviewJobSummary>>>,
        finished_runs: Arc<Mutex<Vec<FinishReviewRun>>>,
        cancelled_turns: Arc<Mutex<Vec<(Uuid, Uuid)>>>,
        deleted_agents: Arc<Mutex<Vec<Uuid>>>,
        git_provider: mai_protocol::GitProvider,
    }

    impl FakeWorkerOps {
        fn new(project_id: Uuid) -> Self {
            Self {
                project: Arc::new(ProjectRecord::new(test_project_summary(project_id))),
                states: Arc::new(Mutex::new(Vec::new())),
                selector_started: Arc::new(Notify::new()),
                release_selector: Arc::new(Notify::new()),
                selector_calls: Arc::new(Mutex::new(0)),
                selector_behaviors: Arc::new(Mutex::new(Vec::new())),
                relay_selection_calls: Arc::new(Mutex::new(Vec::new())),
                failed_relay_prs: Arc::new(Mutex::new(Vec::new())),
                ineligible_relay_prs: Arc::new(Mutex::new(Vec::new())),
                failed_enqueue_prs: Arc::new(Mutex::new(Vec::new())),
                cache_ready_errors: Arc::new(Mutex::new(Vec::new())),
                reviewed_prs: Arc::new(Mutex::new(Vec::new())),
                review_errors: Arc::new(Mutex::new(Vec::new())),
                auto_reviewers: Arc::new(Mutex::new(Vec::new())),
                review_runs: Arc::new(Mutex::new(Vec::new())),
                review_jobs: Arc::new(Mutex::new(Vec::new())),
                finished_runs: Arc::new(Mutex::new(Vec::new())),
                cancelled_turns: Arc::new(Mutex::new(Vec::new())),
                deleted_agents: Arc::new(Mutex::new(Vec::new())),
                git_provider: mai_protocol::GitProvider::Github,
            }
        }

        fn with_git_provider(mut self, git_provider: mai_protocol::GitProvider) -> Self {
            self.git_provider = git_provider;
            self
        }

        async fn push_selector_behavior(&self, behavior: FakeSelectorBehavior) {
            self.selector_behaviors.lock().await.push(behavior);
        }

        async fn push_review_error(&self, error: impl Into<String>) {
            self.review_errors.lock().await.push(error.into());
        }

        async fn push_cache_ready_error(&self, error: impl Into<String>) {
            self.cache_ready_errors.lock().await.push(error.into());
        }

        async fn set_auto_reviewers(&self, reviewers: Vec<mai_protocol::AgentSummary>) {
            *self.auto_reviewers.lock().await = reviewers;
        }

        async fn set_review_runs(&self, runs: Vec<ProjectReviewRunSummary>) {
            *self.review_runs.lock().await = runs;
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
            self.auto_reviewers.lock().await.clone()
        }

        async fn load_project_review_runs(
            &self,
            _project_id: Uuid,
            _offset: usize,
            _limit: usize,
        ) -> crate::Result<Vec<ProjectReviewRunSummary>> {
            Ok(self.review_runs.lock().await.clone())
        }

        async fn finish_project_review_run(&self, request: FinishReviewRun) -> crate::Result<()> {
            self.finished_runs.lock().await.push(request);
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
            let next_review_after_secs = update
                .next_review_at
                .map(|next| (next - chrono::Utc::now()).num_seconds());
            self.states.lock().await.push(ReviewStateSnapshot {
                status: status.clone(),
                next_review_at_set: update.next_review_at.is_some(),
                next_review_after_secs,
                outcome: update.outcome.clone(),
                error: update.error.clone(),
            });
            let mut summary = self.project.summary.write().await;
            summary.review_status = status;
            match update.current_reviewer_agent_id {
                crate::projects::review::state::ReviewerAgentUpdate::Clear => {
                    summary.current_reviewer_agent_id = None;
                }
                crate::projects::review::state::ReviewerAgentUpdate::Keep => {}
                crate::projects::review::state::ReviewerAgentUpdate::Set(reviewer_id) => {
                    summary.current_reviewer_agent_id = Some(reviewer_id);
                }
            }
            summary.next_review_at = update.next_review_at;
            if let Some(outcome) = update.outcome {
                summary.last_review_outcome = Some(outcome);
            }
            summary.review_last_error = update.error;
            Ok(summary.clone())
        }

        async fn ensure_project_repository_ready(&self, _project_id: Uuid) -> crate::Result<()> {
            let error = {
                let mut errors = self.cache_ready_errors.lock().await;
                (!errors.is_empty()).then(|| errors.remove(0))
            };
            if let Some(error) = error {
                return Err(crate::RuntimeError::InvalidInput(error));
            }
            Ok(())
        }

        async fn project_git_provider(
            &self,
            _project_id: Uuid,
        ) -> crate::Result<Option<mai_protocol::GitProvider>> {
            Ok(Some(self.git_provider.clone()))
        }

        async fn run_project_review_selector(
            &self,
            _project_id: Uuid,
            _cancellation_token: CancellationToken,
        ) -> crate::Result<ProjectReviewSelectorRunResult> {
            *self.selector_calls.lock().await += 1;
            self.selector_started.notify_waiters();
            let behavior = {
                let mut behaviors = self.selector_behaviors.lock().await;
                (!behaviors.is_empty()).then(|| behaviors.remove(0))
            };
            if let Some(behavior) = behavior {
                return match behavior {
                    FakeSelectorBehavior::NoEligiblePr => {
                        Ok(ProjectReviewSelectorRunResult::NoEligiblePr)
                    }
                    FakeSelectorBehavior::Queued => Ok(ProjectReviewSelectorRunResult::Queued {
                        selected: Vec::new(),
                        queue: crate::ProjectReviewQueueSummary::default(),
                    }),
                    FakeSelectorBehavior::Error(message) => {
                        Err(crate::RuntimeError::InvalidInput(message.to_string()))
                    }
                };
            }
            self.release_selector.notified().await;
            Ok(ProjectReviewSelectorRunResult::NoEligiblePr)
        }

        async fn select_project_review_pr(
            &self,
            _project_id: Uuid,
            pr: u64,
            head_sha_hint: Option<String>,
        ) -> crate::Result<Option<SelectedProjectReviewPr>> {
            self.relay_selection_calls.lock().await.push(pr);
            if self.failed_relay_prs.lock().await.contains(&pr) {
                return Err(crate::RuntimeError::InvalidInput(format!(
                    "failed relay pr {pr}"
                )));
            }
            if self.ineligible_relay_prs.lock().await.contains(&pr) {
                return Ok(None);
            }
            Ok(Some(SelectedProjectReviewPr {
                pr,
                head_sha: head_sha_hint,
            }))
        }

        async fn enqueue_project_review_signals(
            &self,
            _project_id: Uuid,
            signals: Vec<ProjectReviewSignalInput>,
        ) -> crate::Result<crate::ProjectReviewQueueSummary> {
            let failed_enqueue_prs = self.failed_enqueue_prs.lock().await;
            if signals
                .iter()
                .any(|signal| failed_enqueue_prs.contains(&signal.pr))
            {
                return Err(crate::RuntimeError::InvalidInput(
                    "failed to enqueue review signal".to_string(),
                ));
            }
            let summary = self.project.review_pool.lock().await.enqueue_many(signals);
            self.project.review_notify.notify_waiters();
            Ok(summary.into())
        }

        async fn run_project_review_once(
            &self,
            _project_id: Uuid,
            _cancellation_token: CancellationToken,
            request: crate::projects::review::target::ProjectReviewRequest,
        ) -> crate::Result<ProjectReviewCycleResult> {
            let target_pr = Some(request.pr);
            self.reviewed_prs.lock().await.push(target_pr);
            let error = {
                let mut errors = self.review_errors.lock().await;
                (!errors.is_empty()).then(|| errors.remove(0))
            };
            if let Some(error) = error {
                return Ok(ProjectReviewCycleResult {
                    outcome: ProjectReviewOutcome::Failed,
                    review_event: None,
                    pr: target_pr,
                    summary: Some("Review could not be completed.".to_string()),
                    error: Some(error),
                    failure: None,
                });
            }
            Ok(ProjectReviewCycleResult {
                outcome: ProjectReviewOutcome::ReviewSubmitted,
                review_event: Some(ProjectReviewDecision::Approve),
                pr: target_pr,
                summary: None,
                error: None,
                failure: None,
            })
        }

        async fn project_has_active_review_jobs(&self, _project_id: Uuid) -> crate::Result<bool> {
            Ok(self
                .review_jobs
                .lock()
                .await
                .iter()
                .any(|job| !job.status.is_terminal())
                || !self.project.review_pool.lock().await.is_empty())
        }

        async fn claim_due_project_review_job(
            &self,
            project_id: Uuid,
            owner: String,
            current_time: chrono::DateTime<chrono::Utc>,
            lease_expires_at: chrono::DateTime<chrono::Utc>,
        ) -> crate::Result<Option<ProjectReviewJobSummary>> {
            let mut jobs = self.review_jobs.lock().await;
            let position = jobs.iter().position(|job| {
                !job.status.is_terminal()
                    && job
                        .next_attempt_at
                        .is_none_or(|next_attempt_at| next_attempt_at <= current_time)
            });
            let position = match position {
                Some(position) => position,
                None => {
                    let Some(signal) = self.project.review_pool.lock().await.next() else {
                        return Ok(None);
                    };
                    jobs.push(crate::projects::review::job::new_project_review_job(
                        crate::projects::review::job::NewProjectReviewJob {
                            project_id,
                            pr: signal.pr,
                            head_sha: signal.head_sha.unwrap_or_else(|| "head".to_string()),
                            source: ProjectReviewJobSource::Manual,
                            delivery_id: signal.delivery_id,
                            reason: signal.reason,
                        },
                    ));
                    jobs.len() - 1
                }
            };
            let job = &mut jobs[position];
            job.status = ProjectReviewJobStatus::Preparing;
            job.attempt_count += 1;
            job.next_attempt_at = None;
            job.lease_owner = Some(owner);
            job.lease_expires_at = Some(lease_expires_at);
            Ok(Some(job.clone()))
        }

        async fn load_project_review_job(
            &self,
            _project_id: Uuid,
            job_id: Uuid,
        ) -> crate::Result<Option<ProjectReviewJobSummary>> {
            Ok(self
                .review_jobs
                .lock()
                .await
                .iter()
                .find(|job| job.id == job_id)
                .cloned())
        }

        async fn load_active_project_review_job(
            &self,
            _project_id: Uuid,
        ) -> crate::Result<Option<ProjectReviewJobSummary>> {
            Ok(self
                .review_jobs
                .lock()
                .await
                .iter()
                .filter(|job| !job.status.is_terminal())
                .min_by_key(|job| {
                    let priority = match job.status {
                        ProjectReviewJobStatus::Reconciling => 0,
                        ProjectReviewJobStatus::SubmissionPending => 1,
                        ProjectReviewJobStatus::Running => 2,
                        ProjectReviewJobStatus::Preparing => 3,
                        ProjectReviewJobStatus::Queued => 4,
                        ProjectReviewJobStatus::RetryWaiting => 5,
                        ProjectReviewJobStatus::Succeeded
                        | ProjectReviewJobStatus::Failed
                        | ProjectReviewJobStatus::Cancelled
                        | ProjectReviewJobStatus::Superseded => 6,
                    };
                    (priority, job.created_at)
                })
                .cloned())
        }

        async fn save_claimed_project_review_job(
            &self,
            job: ProjectReviewJobSummary,
            _owner: String,
        ) -> crate::Result<bool> {
            let mut jobs = self.review_jobs.lock().await;
            if let Some(existing) = jobs.iter_mut().find(|existing| existing.id == job.id) {
                *existing = job;
                return Ok(true);
            }
            Ok(false)
        }

        async fn heartbeat_project_review_job(
            &self,
            _job_id: Uuid,
            _owner: String,
            _updated_at: chrono::DateTime<chrono::Utc>,
            _lease_expires_at: chrono::DateTime<chrono::Utc>,
        ) -> crate::Result<bool> {
            Ok(true)
        }

        async fn recover_expired_project_review_jobs(
            &self,
            now: chrono::DateTime<chrono::Utc>,
        ) -> crate::Result<usize> {
            let mut jobs = self.review_jobs.lock().await;
            let mut recovered = 0;
            for job in jobs.iter_mut() {
                if matches!(
                    job.status,
                    ProjectReviewJobStatus::Preparing
                        | ProjectReviewJobStatus::Running
                        | ProjectReviewJobStatus::SubmissionPending
                        | ProjectReviewJobStatus::Reconciling
                ) && job
                    .lease_expires_at
                    .is_none_or(|expires_at| expires_at <= now)
                {
                    job.status = if job.submission_intent.is_some() {
                        ProjectReviewJobStatus::Reconciling
                    } else {
                        ProjectReviewJobStatus::RetryWaiting
                    };
                    job.next_attempt_at = Some(now);
                    job.lease_owner = None;
                    job.lease_expires_at = None;
                    job.updated_at = now;
                    recovered += 1;
                }
            }
            Ok(recovered)
        }

        async fn cancel_active_project_review_jobs(
            &self,
            _project_id: Uuid,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> crate::Result<usize> {
            Ok(0)
        }

        async fn run_project_review_job_attempt(
            &self,
            job: ProjectReviewJobSummary,
            _owner: String,
            cancellation_token: CancellationToken,
        ) -> crate::Result<ProjectReviewCycleResult> {
            let job_id = job.id;
            let job_head = job.head_sha.clone();
            let mut result = self
                .run_project_review_once(
                    job.project_id,
                    cancellation_token,
                    crate::projects::review::target::ProjectReviewRequest {
                        pr: job.pr,
                        head_sha_hint: Some(job.head_sha),
                    },
                )
                .await?;
            if result.error.is_some() {
                let code = result
                    .error
                    .as_deref()
                    .filter(|message| *message == "server_is_overloaded")
                    .map(str::to_string);
                result.failure = Some(mai_protocol::ProjectReviewFailure {
                    category: ProjectReviewFailureCategory::ProviderCapacity,
                    code,
                    http_status: None,
                    message: result.error.clone().unwrap_or_default(),
                    retry: pl_protocol::RetryDisposition::Retryable {
                        retry_after_ms: None,
                    },
                });
            }
            if result.outcome == ProjectReviewOutcome::ReviewSubmitted
                && let Some(stored) = self
                    .review_jobs
                    .lock()
                    .await
                    .iter_mut()
                    .find(|stored| stored.id == job_id)
            {
                stored.status = ProjectReviewJobStatus::Succeeded;
                stored.submission_receipt = Some(mai_protocol::ProjectReviewSubmissionReceipt {
                    github_review_id: 42,
                    event: result
                        .review_event
                        .clone()
                        .unwrap_or(ProjectReviewDecision::Comment),
                    head_sha: job_head,
                    html_url: None,
                    submitted_at: now(),
                });
                stored.finished_at = Some(now());
                stored.lease_owner = None;
                stored.lease_expires_at = None;
            }
            Ok(result)
        }

        async fn reconcile_project_review_job(
            &self,
            _job: ProjectReviewJobSummary,
        ) -> crate::Result<Option<mai_protocol::ProjectReviewSubmissionReceipt>> {
            Ok(None)
        }

        async fn agent_current_turn(&self, agent_id: Uuid) -> crate::Result<Option<Uuid>> {
            Ok(self
                .auto_reviewers
                .lock()
                .await
                .iter()
                .find(|agent| agent.id == agent_id)
                .and_then(|agent| agent.state.active_turn()))
        }

        async fn cancel_agent_turn(&self, agent_id: Uuid, turn_id: Uuid) -> crate::Result<()> {
            self.cancelled_turns.lock().await.push((agent_id, turn_id));
            Ok(())
        }

        async fn find_project_review_job_reviewer(
            &self,
            job: ProjectReviewJobSummary,
        ) -> crate::Result<Option<Uuid>> {
            Ok(job.reviewer_agent_id)
        }

        async fn delete_agent(&self, agent_id: Uuid) -> crate::Result<()> {
            self.deleted_agents.lock().await.push(agent_id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn app_selector_status_is_visible_while_selecting_and_waiting_when_empty() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        let task_ops = ops.clone();
        let selector_task = tokio::spawn(async move {
            let context = ProjectReviewTaskContext {
                ops: task_ops,
                project_id,
                cancellation_token: CancellationToken::new(),
            };
            run_selector_attempt(&context, std::time::Duration::ZERO)
                .await
                .expect("run selector");
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
                    next_review_after_secs: None,
                    outcome: None,
                    error: None,
                },
                ReviewStateSnapshot {
                    status: ProjectReviewStatus::Waiting,
                    next_review_at_set: false,
                    next_review_after_secs: None,
                    outcome: Some(ProjectReviewOutcome::NoEligiblePr),
                    error: None,
                },
            ],
            states
        );
    }

    #[tokio::test]
    async fn selector_attempt_uses_caller_retry_delay_for_errors() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        ops.push_selector_behavior(FakeSelectorBehavior::Error("selector boom"))
            .await;

        let context = ProjectReviewTaskContext {
            ops: ops.clone(),
            project_id,
            cancellation_token: CancellationToken::new(),
        };
        let result = run_selector_attempt(&context, std::time::Duration::from_secs(1)).await;

        assert!(result.is_err());
        let states = ops.states.lock().await.clone();
        assert_eq!(ProjectReviewStatus::Selecting, states[0].status);
        assert_eq!(ProjectReviewStatus::Waiting, states[1].status);
        assert_eq!(
            Some("invalid input: selector boom".to_string()),
            states[1].error
        );
        assert_delay_near(states[1].next_review_after_secs, 1);
    }

    #[test]
    fn selector_retry_backoff_starts_at_one_second_and_doubles() {
        let mut backoff = super::super::project_review_retry_backoff();

        assert_eq!(std::time::Duration::from_secs(1), backoff.next_delay());
        assert_eq!(std::time::Duration::from_secs(2), backoff.next_delay());
        assert_eq!(std::time::Duration::from_secs(4), backoff.next_delay());
    }

    #[tokio::test]
    async fn selector_attempt_uses_scan_interval_after_no_eligible_pr() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        ops.push_selector_behavior(FakeSelectorBehavior::NoEligiblePr)
            .await;

        let context = ProjectReviewTaskContext {
            ops: ops.clone(),
            project_id,
            cancellation_token: CancellationToken::new(),
        };
        run_selector_attempt(&context, std::time::Duration::from_secs(1800))
            .await
            .expect("selector attempt");

        let states = ops.states.lock().await.clone();
        assert_eq!(ProjectReviewStatus::Waiting, states[1].status);
        assert_eq!(Some(ProjectReviewOutcome::NoEligiblePr), states[1].outcome);
        assert_delay_near(states[1].next_review_after_secs, 1800);
    }

    #[tokio::test]
    async fn selector_attempt_sets_idle_after_queued_result() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        ops.push_selector_behavior(FakeSelectorBehavior::Queued)
            .await;

        let context = ProjectReviewTaskContext {
            ops: ops.clone(),
            project_id,
            cancellation_token: CancellationToken::new(),
        };
        run_selector_attempt(&context, std::time::Duration::from_secs(1800))
            .await
            .expect("selector attempt");

        let states = ops.states.lock().await.clone();
        assert_eq!(
            vec![
                ReviewStateSnapshot {
                    status: ProjectReviewStatus::Selecting,
                    next_review_at_set: false,
                    next_review_after_secs: None,
                    outcome: None,
                    error: None,
                },
                ReviewStateSnapshot {
                    status: ProjectReviewStatus::Idle,
                    next_review_at_set: false,
                    next_review_after_secs: None,
                    outcome: None,
                    error: None,
                },
            ],
            states
        );
    }

    #[tokio::test]
    async fn selector_loop_continues_after_app_no_eligible_pr_success() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id)
            .with_git_provider(mai_protocol::GitProvider::GithubAppRelay);
        ops.push_selector_behavior(FakeSelectorBehavior::NoEligiblePr)
            .await;
        ops.push_selector_behavior(FakeSelectorBehavior::NoEligiblePr)
            .await;
        let token = CancellationToken::new();
        let context = ProjectReviewTaskContext {
            ops: ops.clone(),
            project_id,
            cancellation_token: token.clone(),
        };
        let selector_task = tokio::spawn(async move {
            super::run_project_review_selector_loop_with_interval(
                context,
                std::time::Duration::from_millis(5),
            )
            .await;
        });

        for _ in 0..20 {
            if *ops.selector_calls.lock().await >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        assert_eq!(2, *ops.selector_calls.lock().await);
        token.cancel();
        selector_task.await.expect("selector loop task");
    }

    #[tokio::test]
    async fn pool_worker_waits_for_pool_and_does_not_run_selector() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        let token = CancellationToken::new();
        let task_ops = ops.clone();
        let task_token = token.clone();
        let worker_task = tokio::spawn(async move {
            super::run_project_review_loop(task_ops, project_id, task_token).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(0, *ops.selector_calls.lock().await);
        assert_eq!(Vec::<Option<u64>>::new(), *ops.reviewed_prs.lock().await);

        {
            let mut pool = ops.project.review_pool.lock().await;
            pool.enqueue_many([ProjectReviewSignalInput {
                pr: 42,
                head_sha: Some("head-42".to_string()),
                delivery_id: None,
                reason: "test".to_string(),
            }]);
        }
        ops.project.review_notify.notify_waiters();

        for _ in 0..20 {
            if !ops.reviewed_prs.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(vec![Some(42)], *ops.reviewed_prs.lock().await);
        assert_eq!(0, *ops.selector_calls.lock().await);

        token.cancel();
        worker_task.await.expect("worker task");
    }

    #[tokio::test]
    async fn pool_worker_wakes_startup_cache_backoff_when_review_is_queued() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        ops.push_cache_ready_error("project cache volume sync failed")
            .await;
        let token = CancellationToken::new();
        let task_ops = ops.clone();
        let task_token = token.clone();
        let worker_task = tokio::spawn(async move {
            super::run_project_review_loop(task_ops, project_id, task_token).await;
        });

        for _ in 0..20 {
            if !ops.states.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        {
            let mut pool = ops.project.review_pool.lock().await;
            pool.enqueue_many([ProjectReviewSignalInput {
                pr: 42,
                head_sha: Some("head-42".to_string()),
                delivery_id: None,
                reason: "manual_rereview".to_string(),
            }]);
        }
        ops.project.review_notify.notify_waiters();

        for _ in 0..20 {
            if !ops.reviewed_prs.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(vec![Some(42)], *ops.reviewed_prs.lock().await);

        token.cancel();
        worker_task.await.expect("worker task");
    }

    #[tokio::test]
    async fn pool_worker_persists_retry_waiting_after_retryable_model_failure() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        ops.push_review_error("model error: request to https://token-plan-cn.xiaomimimo.com/v1/chat/completions returned 500 Internal Server Error: {\"error\":{\"message\":\"<html><body><h1>502 Bad Gateway</h1></body></html>\"}}")
            .await;
        let token = CancellationToken::new();
        let task_ops = ops.clone();
        let task_token = token.clone();
        let worker_task = tokio::spawn(async move {
            super::run_project_review_loop(task_ops, project_id, task_token).await;
        });

        {
            let mut pool = ops.project.review_pool.lock().await;
            pool.enqueue_many([ProjectReviewSignalInput {
                pr: 42,
                head_sha: Some("head-42".to_string()),
                delivery_id: Some("delivery-42".to_string()),
                reason: "test".to_string(),
            }]);
        }
        ops.project.review_notify.notify_waiters();

        for _ in 0..20 {
            if !ops.reviewed_prs.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(vec![Some(42)], *ops.reviewed_prs.lock().await);
        let jobs = ops.review_jobs.lock().await.clone();
        assert_eq!(1, jobs.len());
        assert_eq!(ProjectReviewJobStatus::RetryWaiting, jobs[0].status);
        assert_eq!(42, jobs[0].pr);
        assert_eq!("head-42", jobs[0].head_sha);
        assert_eq!(Some("delivery-42"), jobs[0].delivery_id.as_deref());
        assert!(jobs[0].next_attempt_at.is_some());

        let states = ops.states.lock().await.clone();
        let last = states.last().expect("review state");
        assert_eq!(ProjectReviewStatus::RetryWaiting, last.status);
        assert_eq!(Some(ProjectReviewOutcome::Failed), last.outcome);
        assert!(last.next_review_at_set);
        assert!(
            last.error
                .as_deref()
                .is_some_and(|error| error.contains("502 Bad Gateway"))
        );

        token.cancel();
        worker_task.await.expect("worker task");
    }

    #[tokio::test]
    async fn pool_worker_recovers_after_repeated_provider_overload_without_replacing_job() {
        let project_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        for _ in 0..3 {
            ops.push_review_error("server_is_overloaded").await;
        }
        let mut job = crate::projects::review::job::new_project_review_job(
            crate::projects::review::job::NewProjectReviewJob {
                project_id,
                pr: 42,
                head_sha: "head-42".to_string(),
                source: ProjectReviewJobSource::Manual,
                delivery_id: None,
                reason: "overload fault injection".to_string(),
            },
        );
        let job_id = job.id;
        job.reviewer_agent_id = Some(reviewer_id);
        ops.review_jobs.lock().await.push(job);

        let token = CancellationToken::new();
        let task_ops = ops.clone();
        let task_token = token.clone();
        let worker_task = tokio::spawn(async move {
            super::run_project_review_loop(task_ops, project_id, task_token).await;
        });

        for expected_attempts in 1..=3 {
            for _ in 0..100 {
                let jobs = ops.review_jobs.lock().await;
                if jobs[0].attempt_count == expected_attempts
                    && jobs[0].status == ProjectReviewJobStatus::RetryWaiting
                {
                    break;
                }
                drop(jobs);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let mut jobs = ops.review_jobs.lock().await;
            assert_eq!(expected_attempts, jobs[0].attempt_count);
            assert_eq!(ProjectReviewJobStatus::RetryWaiting, jobs[0].status);
            assert_eq!(Some(reviewer_id), jobs[0].reviewer_agent_id);
            assert_eq!(
                Some("server_is_overloaded"),
                jobs[0]
                    .failure
                    .as_ref()
                    .and_then(|failure| failure.code.as_deref())
            );
            jobs[0].next_attempt_at = Some(now() - chrono::TimeDelta::seconds(1));
            drop(jobs);
            ops.project.review_notify.notify_waiters();
        }

        for _ in 0..100 {
            if ops.review_jobs.lock().await[0].status == ProjectReviewJobStatus::Succeeded {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let jobs = ops.review_jobs.lock().await;
        assert_eq!(1, jobs.len());
        assert_eq!(job_id, jobs[0].id);
        assert_eq!(4, jobs[0].attempt_count);
        assert_eq!(Some(reviewer_id), jobs[0].reviewer_agent_id);
        assert_eq!(ProjectReviewJobStatus::Succeeded, jobs[0].status);
        assert_eq!(
            Some(42),
            jobs[0]
                .submission_receipt
                .as_ref()
                .map(|receipt| receipt.github_review_id)
        );
        drop(jobs);
        assert_eq!(vec![Some(42); 4], *ops.reviewed_prs.lock().await);

        token.cancel();
        worker_task.await.expect("worker task");
    }

    #[tokio::test]
    async fn project_projection_keeps_running_job_visible_when_another_job_is_queued() {
        let project_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        let mut running = crate::projects::review::job::new_project_review_job(
            crate::projects::review::job::NewProjectReviewJob {
                project_id,
                pr: 41,
                head_sha: "head-running".to_string(),
                source: ProjectReviewJobSource::Automatic,
                delivery_id: None,
                reason: "running".to_string(),
            },
        );
        running.status = ProjectReviewJobStatus::Running;
        running.reviewer_agent_id = Some(reviewer_id);
        let queued = crate::projects::review::job::new_project_review_job(
            crate::projects::review::job::NewProjectReviewJob {
                project_id,
                pr: 42,
                head_sha: "head-queued".to_string(),
                source: ProjectReviewJobSource::Automatic,
                delivery_id: None,
                reason: "queued".to_string(),
            },
        );
        *ops.review_jobs.lock().await = vec![running.clone(), queued.clone()];

        crate::projects::review::job_worker::refresh_project_review_job_projection(
            &ops, project_id, &queued, None, None,
        )
        .await;
        {
            let project = ops.project.summary.read().await;
            assert_eq!(ProjectReviewStatus::Running, project.review_status);
            assert_eq!(Some(reviewer_id), project.current_reviewer_agent_id);
        }

        ops.review_jobs.lock().await[0].status = ProjectReviewJobStatus::Succeeded;
        crate::projects::review::job_worker::refresh_project_review_job_projection(
            &ops, project_id, &running, None, None,
        )
        .await;
        let project = ops.project.summary.read().await;
        assert_eq!(ProjectReviewStatus::Queued, project.review_status);
        assert_eq!(None, project.current_reviewer_agent_id);
    }

    #[tokio::test]
    async fn pool_worker_recovers_lease_that_expires_after_scheduler_startup() {
        let project_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        let mut interrupted = crate::projects::review::job::new_project_review_job(
            crate::projects::review::job::NewProjectReviewJob {
                project_id,
                pr: 42,
                head_sha: "head-42".to_string(),
                source: ProjectReviewJobSource::Automatic,
                delivery_id: None,
                reason: "restart recovery".to_string(),
            },
        );
        let job_id = interrupted.id;
        interrupted.status = ProjectReviewJobStatus::Running;
        interrupted.attempt_count = 1;
        interrupted.reviewer_agent_id = Some(reviewer_id);
        interrupted.lease_owner = Some("stopped-instance".to_string());
        interrupted.lease_expires_at = Some(now() - chrono::TimeDelta::milliseconds(1));
        ops.review_jobs.lock().await.push(interrupted);

        let token = CancellationToken::new();
        let task_ops = ops.clone();
        let task_token = token.clone();
        let worker_task = tokio::spawn(async move {
            super::run_project_review_loop(task_ops, project_id, task_token).await;
        });

        for _ in 0..100 {
            if ops.review_jobs.lock().await[0].status == ProjectReviewJobStatus::Succeeded {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let jobs = ops.review_jobs.lock().await;
        assert_eq!(1, jobs.len());
        assert_eq!(job_id, jobs[0].id);
        assert_eq!(2, jobs[0].attempt_count);
        assert_eq!(Some(reviewer_id), jobs[0].reviewer_agent_id);
        assert_eq!(ProjectReviewJobStatus::Succeeded, jobs[0].status);
        assert!(jobs[0].submission_receipt.is_some());
        drop(jobs);

        token.cancel();
        worker_task.await.expect("worker task");
    }

    #[tokio::test]
    async fn pool_worker_uses_persisted_job_instead_of_projection_as_queue_lock() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        let reviewer = test_reviewer_agent(
            project_id,
            ops.project.summary.read().await.maintainer_agent_id,
            TestReviewerState::WaitingTool,
        );
        let reviewer_id = reviewer.id;
        let turn_id = reviewer.state.active_turn();
        {
            let mut summary = ops.project.summary.write().await;
            summary.review_status = ProjectReviewStatus::Running;
            summary.current_reviewer_agent_id = Some(reviewer_id);
        }
        ops.set_auto_reviewers(vec![reviewer]).await;
        ops.set_review_runs(vec![test_review_run(
            project_id,
            Some(reviewer_id),
            turn_id,
            ProjectReviewRunStatus::Running,
        )])
        .await;
        let token = CancellationToken::new();
        let task_ops = ops.clone();
        let task_token = token.clone();
        let worker_task = tokio::spawn(async move {
            super::run_project_review_loop(task_ops, project_id, task_token).await;
        });

        {
            let mut pool = ops.project.review_pool.lock().await;
            pool.enqueue_many([ProjectReviewSignalInput {
                pr: 42,
                head_sha: Some("head-42".to_string()),
                delivery_id: Some("delivery-42".to_string()),
                reason: "test".to_string(),
            }]);
        }
        ops.project.review_notify.notify_waiters();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(vec![Some(42)], *ops.reviewed_prs.lock().await);

        token.cancel();
        worker_task.await.expect("worker task");
    }

    #[tokio::test]
    async fn pool_worker_does_not_opportunistically_destroy_unowned_reviewer() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        let maintainer_agent_id = ops.project.summary.read().await.maintainer_agent_id;
        let reviewer =
            test_reviewer_agent(project_id, maintainer_agent_id, TestReviewerState::Deleting);
        let reviewer_id = reviewer.id;
        let turn_id = reviewer.state.active_turn().expect("reviewer turn");
        ops.set_auto_reviewers(vec![reviewer]).await;
        ops.set_review_runs(vec![test_review_run(
            project_id,
            Some(reviewer_id),
            Some(turn_id),
            ProjectReviewRunStatus::Running,
        )])
        .await;

        let token = CancellationToken::new();
        let task_ops = ops.clone();
        let task_token = token.clone();
        let worker_task = tokio::spawn(async move {
            super::run_project_review_loop(task_ops, project_id, task_token).await;
        });

        {
            let mut pool = ops.project.review_pool.lock().await;
            pool.enqueue_many([ProjectReviewSignalInput {
                pr: 42,
                head_sha: Some("head-42".to_string()),
                delivery_id: Some("delivery-42".to_string()),
                reason: "test".to_string(),
            }]);
        }
        ops.project.review_notify.notify_waiters();

        for _ in 0..20 {
            if !ops.reviewed_prs.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(vec![Some(42)], *ops.reviewed_prs.lock().await);
        assert_eq!(
            Vec::<(Uuid, Uuid)>::new(),
            *ops.cancelled_turns.lock().await
        );
        assert_eq!(Vec::<Uuid>::new(), *ops.deleted_agents.lock().await);
        assert!(ops.finished_runs.lock().await.is_empty());

        token.cancel();
        worker_task.await.expect("worker task");
    }

    #[tokio::test]
    async fn runtime_repair_keeps_consistent_reviewer_and_deletes_extra_stale_reviewer() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        let maintainer_agent_id = ops.project.summary.read().await.maintainer_agent_id;
        let reviewer =
            test_reviewer_agent(project_id, maintainer_agent_id, TestReviewerState::Running);
        let reviewer_id = reviewer.id;
        let extra_reviewer =
            test_reviewer_agent(project_id, maintainer_agent_id, TestReviewerState::Failed);
        let extra_reviewer_id = extra_reviewer.id;
        {
            let mut summary = ops.project.summary.write().await;
            summary.review_status = ProjectReviewStatus::Running;
            summary.current_reviewer_agent_id = Some(reviewer_id);
        }
        ops.set_auto_reviewers(vec![reviewer, extra_reviewer]).await;
        ops.set_review_runs(vec![
            test_review_run(
                project_id,
                Some(extra_reviewer_id),
                None,
                ProjectReviewRunStatus::Running,
            ),
            test_review_run(
                project_id,
                Some(reviewer_id),
                None,
                ProjectReviewRunStatus::Running,
            ),
        ])
        .await;

        repair_project_review_singleton(&ops, project_id, 10, ProjectReviewRepairReason::Runtime)
            .await
            .expect("repair");

        let finished = ops.finished_runs.lock().await.clone();
        assert_eq!(1, finished.len());
        assert_eq!(Some(extra_reviewer_id), finished[0].reviewer_agent_id);
        assert_eq!(ProjectReviewRunStatus::Cancelled, finished[0].status);
        assert_eq!(vec![extra_reviewer_id], *ops.deleted_agents.lock().await);
        assert_eq!(Vec::<ReviewStateSnapshot>::new(), *ops.states.lock().await);
    }

    #[tokio::test]
    async fn startup_repair_purges_closed_reviewer_tombstone() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        let maintainer_agent_id = ops.project.summary.read().await.maintainer_agent_id;
        let reviewer =
            test_reviewer_agent(project_id, maintainer_agent_id, TestReviewerState::Deleted);
        let reviewer_id = reviewer.id;
        ops.set_auto_reviewers(vec![reviewer]).await;

        repair_project_review_singleton(&ops, project_id, 10, ProjectReviewRepairReason::Startup)
            .await
            .expect("startup repair");

        assert_eq!(vec![reviewer_id], *ops.deleted_agents.lock().await);
    }

    #[tokio::test]
    async fn stop_project_review_loop_deletes_active_reviewer_when_state_lost_reviewer_id() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        let maintainer_agent_id = ops.project.summary.read().await.maintainer_agent_id;
        let reviewer =
            test_reviewer_agent(project_id, maintainer_agent_id, TestReviewerState::Running);
        let reviewer_id = reviewer.id;
        let turn_id = reviewer.state.active_turn().expect("reviewer turn");
        ops.set_auto_reviewers(vec![reviewer]).await;

        super::stop_project_review_loop(ops.clone(), project_id, 10).await;

        assert_eq!(
            vec![(reviewer_id, turn_id)],
            *ops.cancelled_turns.lock().await
        );
        assert_eq!(vec![reviewer_id], *ops.deleted_agents.lock().await);
    }

    #[tokio::test]
    async fn relay_selector_moves_eligible_relay_signal_to_pr_pool() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        let token = CancellationToken::new();
        let task_ops = ops.clone();
        let task_token = token.clone();
        let relay_task = tokio::spawn(async move {
            super::run_project_review_relay_selector_loop_for_test(
                task_ops, project_id, task_token,
            )
            .await;
        });

        {
            let mut queue = ops.project.relay_review_queue.lock().await;
            queue.enqueue_many([ProjectReviewRelaySignalInput {
                pr: 33,
                head_sha: Some("head-33".to_string()),
                delivery_id: Some("delivery-33".to_string()),
                reason: "check_run".to_string(),
            }]);
        }
        ops.project.relay_review_notify.notify_waiters();

        for _ in 0..20 {
            if !ops.relay_selection_calls.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(vec![33], *ops.relay_selection_calls.lock().await);
        let pending = ops
            .project
            .review_pool
            .lock()
            .await
            .next()
            .expect("eligible relay signal entered review pool");
        assert_eq!(33, pending.pr);
        assert_eq!(Some("head-33".to_string()), pending.head_sha);
        assert_eq!(Some("delivery-33".to_string()), pending.delivery_id);
        assert_eq!("check_run", pending.reason);

        token.cancel();
        relay_task.await.expect("relay selector task");
    }

    #[tokio::test]
    async fn relay_selector_requeues_failed_signal_and_waits() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        ops.failed_relay_prs.lock().await.push(11);
        let token = CancellationToken::new();
        let task_ops = ops.clone();
        let task_token = token.clone();
        let relay_task = tokio::spawn(async move {
            super::run_project_review_relay_selector_loop_for_test(
                task_ops, project_id, task_token,
            )
            .await;
        });

        {
            let mut queue = ops.project.relay_review_queue.lock().await;
            queue.enqueue_many([
                ProjectReviewRelaySignalInput {
                    pr: 11,
                    head_sha: Some("head-11".to_string()),
                    delivery_id: Some("delivery-11".to_string()),
                    reason: "check_run".to_string(),
                },
                ProjectReviewRelaySignalInput {
                    pr: 12,
                    head_sha: Some("head-12".to_string()),
                    delivery_id: Some("delivery-12".to_string()),
                    reason: "check_run".to_string(),
                },
            ]);
        }
        ops.project.relay_review_notify.notify_waiters();

        for _ in 0..20 {
            if !ops.relay_selection_calls.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(vec![11], *ops.relay_selection_calls.lock().await);
        assert_eq!(None, ops.project.review_pool.lock().await.next());
        let mut queue = ops.project.relay_review_queue.lock().await;
        assert_eq!(Some(11), queue.next().map(|signal| signal.pr));
        assert_eq!(Some(12), queue.next().map(|signal| signal.pr));

        token.cancel();
        relay_task.await.expect("relay selector task");
    }

    #[tokio::test]
    async fn relay_selector_drops_ineligible_signal_without_retry() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        ops.ineligible_relay_prs.lock().await.push(21);
        let token = CancellationToken::new();
        let task_ops = ops.clone();
        let task_token = token.clone();
        let relay_task = tokio::spawn(async move {
            super::run_project_review_relay_selector_loop_for_test(
                task_ops, project_id, task_token,
            )
            .await;
        });

        {
            let mut queue = ops.project.relay_review_queue.lock().await;
            queue.enqueue_many([ProjectReviewRelaySignalInput {
                pr: 21,
                head_sha: Some("head-21".to_string()),
                delivery_id: Some("delivery-21".to_string()),
                reason: "check_run".to_string(),
            }]);
        }
        ops.project.relay_review_notify.notify_waiters();

        for _ in 0..20 {
            if !ops.relay_selection_calls.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(vec![21], *ops.relay_selection_calls.lock().await);
        assert_eq!(None, ops.project.review_pool.lock().await.next());
        assert_eq!(None, ops.project.relay_review_queue.lock().await.next());

        token.cancel();
        relay_task.await.expect("relay selector task");
    }

    #[tokio::test]
    async fn relay_selector_requeues_when_pool_enqueue_fails() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        ops.failed_enqueue_prs.lock().await.push(22);
        let token = CancellationToken::new();
        let task_ops = ops.clone();
        let task_token = token.clone();
        let relay_task = tokio::spawn(async move {
            super::run_project_review_relay_selector_loop_for_test(
                task_ops, project_id, task_token,
            )
            .await;
        });

        {
            let mut queue = ops.project.relay_review_queue.lock().await;
            queue.enqueue_many([ProjectReviewRelaySignalInput {
                pr: 22,
                head_sha: Some("head-22".to_string()),
                delivery_id: Some("delivery-22".to_string()),
                reason: "check_run".to_string(),
            }]);
        }
        ops.project.relay_review_notify.notify_waiters();

        for _ in 0..20 {
            if !ops.relay_selection_calls.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(vec![22], *ops.relay_selection_calls.lock().await);
        assert_eq!(None, ops.project.review_pool.lock().await.next());
        let requeued = ops
            .project
            .relay_review_queue
            .lock()
            .await
            .next()
            .expect("failed enqueue signal was requeued");
        assert_eq!(22, requeued.pr);
        assert_eq!(Some("delivery-22".to_string()), requeued.delivery_id);
        assert_eq!(Some("head-22".to_string()), requeued.head_sha);
        assert_eq!("check_run", requeued.reason);

        token.cancel();
        relay_task.await.expect("relay selector task");
    }

    #[tokio::test]
    async fn manual_queue_starts_only_pool_worker_when_auto_review_is_disabled() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        {
            let mut summary = ops.project.summary.write().await;
            summary.auto_review_enabled = false;
            summary.review_status = ProjectReviewStatus::Disabled;
        }
        ops.project
            .review_pool
            .lock()
            .await
            .enqueue_many([ProjectReviewSignalInput {
                pr: 17,
                head_sha: None,
                delivery_id: None,
                reason: "manual_rereview".to_string(),
            }]);

        super::start_project_review_loop_if_ready(ops.clone(), project_id)
            .await
            .expect("start manual review worker");

        {
            let worker = ops.project.review_worker.lock().await;
            let worker = worker.as_ref().expect("pool worker");
            assert!(worker.selector_abort_handle.is_none());
            assert!(worker.relay_selector_abort_handle.is_none());
        }

        super::stop_project_review_loop(ops, project_id, 10).await;
    }

    #[tokio::test]
    async fn selector_worker_starts_for_github_token_and_github_app_providers() {
        for git_provider in [
            mai_protocol::GitProvider::Github,
            mai_protocol::GitProvider::GithubAppRelay,
        ] {
            let project_id = Uuid::new_v4();
            let ops = FakeWorkerOps::new(project_id).with_git_provider(git_provider);

            super::start_project_review_loop_if_ready(ops.clone(), project_id)
                .await
                .expect("start review tasks");

            ops.selector_started.notified().await;
            assert_eq!(1, *ops.selector_calls.lock().await);
            ops.release_selector.notify_waiters();

            super::stop_project_review_loop(ops, project_id, 10).await;
        }
    }

    #[tokio::test]
    async fn selector_status_does_not_override_active_review() {
        let project_id = Uuid::new_v4();
        let ops = FakeWorkerOps::new(project_id);
        {
            let mut summary = ops.project.summary.write().await;
            summary.review_status = ProjectReviewStatus::Running;
            summary.current_reviewer_agent_id = Some(Uuid::new_v4());
        }
        let task_ops = ops.clone();
        let selector_task = tokio::spawn(async move {
            let context = ProjectReviewTaskContext {
                ops: task_ops,
                project_id,
                cancellation_token: CancellationToken::new(),
            };
            run_selector_attempt(&context, std::time::Duration::ZERO)
                .await
                .expect("run selector");
        });

        ops.selector_started.notified().await;
        ops.release_selector.notify_waiters();
        selector_task.await.expect("selector task");

        assert_eq!(Vec::<ReviewStateSnapshot>::new(), *ops.states.lock().await);
    }

    fn assert_delay_near(actual: Option<i64>, expected: i64) {
        let actual = actual.expect("next review delay");
        assert!(
            (expected - 2..=expected).contains(&actual),
            "expected next review delay near {expected}s, got {actual}s"
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

    #[derive(Debug, Clone, Copy)]
    enum TestReviewerState {
        WaitingTool,
        Deleting,
        Running,
        Failed,
        Deleted,
    }

    fn test_reviewer_agent(
        project_id: Uuid,
        maintainer_agent_id: Uuid,
        test_state: TestReviewerState,
    ) -> mai_protocol::AgentSummary {
        let turn_id = Uuid::new_v4();
        let mut state = AgentState {
            resource: AgentResourceState::Ready,
            runtime: AgentRuntimeState {
                activity: AgentRuntimeActivity::Running,
                active_turn: Some(turn_id),
                active_session: Some(Uuid::new_v4()),
                ..AgentRuntimeState::default()
            },
            ..AgentState::default()
        };
        match test_state {
            TestReviewerState::WaitingTool => {
                state.runtime.activity = AgentRuntimeActivity::WaitingTool;
            }
            TestReviewerState::Deleting => {
                state.resource = AgentResourceState::Deleting;
            }
            TestReviewerState::Running => {}
            TestReviewerState::Failed => {
                state.resource = AgentResourceState::Failed;
                state.resource_error = Some("reviewer failed".to_string());
                state.runtime.activity = AgentRuntimeActivity::Idle;
                state.runtime.active_turn = None;
                state.runtime.active_session = None;
            }
            TestReviewerState::Deleted => {
                state.resource = AgentResourceState::Deleted;
                state.runtime.lifecycle = mai_protocol::AgentRuntimeLifecycle::Closed;
                state.runtime.activity = AgentRuntimeActivity::Idle;
                state.runtime.active_turn = None;
                state.runtime.active_session = None;
            }
        }
        mai_protocol::AgentSummary {
            id: Uuid::new_v4(),
            parent_id: Some(maintainer_agent_id),
            task_id: None,
            project_id: Some(project_id),
            role: Some(AgentRole::Reviewer),
            name: "reviewer".to_string(),
            state,
            container_id: Some("container".to_string()),
            docker_image: "unused".to_string(),
            provider_id: "mock".to_string(),
            provider_name: "Mock".to_string(),
            model: "mock-model".to_string(),
            reasoning_effort: Some("medium".to_string()),
            created_at: now(),
            updated_at: now(),
            token_usage: TokenUsage::default(),
        }
    }

    fn test_review_run(
        project_id: Uuid,
        reviewer_agent_id: Option<Uuid>,
        turn_id: Option<Uuid>,
        status: ProjectReviewRunStatus,
    ) -> ProjectReviewRunSummary {
        ProjectReviewRunSummary {
            id: Uuid::new_v4(),
            job_id: None,
            attempt_index: 1,
            project_id,
            reviewer_agent_id,
            turn_id,
            started_at: now(),
            finished_at: None,
            status,
            outcome: None,
            review_event: None,
            pr: Some(42),
            summary: None,
            error: None,
            failure: None,
            token_usage: TokenUsage::default(),
        }
    }
}
