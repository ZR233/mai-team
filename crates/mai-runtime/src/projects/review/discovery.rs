use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use futures::{StreamExt, stream};
use mai_protocol::{ProjectId, ProjectReviewDiscoverySnapshot, ProjectReviewDiscoveryState};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use super::backoff::ProjectReviewRetryBackoff;
use super::selector::ProjectReviewSelectorRunResult;
use crate::{Result, RuntimeError};

const DISCOVERY_PROJECT_CONCURRENCY: usize = 2;

/// 提供独立 PR discovery 调度所需的项目判定、扫描与状态读写边界。
///
/// 实现方必须将快照保存在 Runtime 内存中，并在更新后发布产品事件；快照只用于
/// 观测，不能反向驱动 Review Job 状态机或下一轮调度。
pub(crate) trait ProjectReviewDiscoveryOps: Clone + Send + Sync + 'static {
    fn project_review_discovery_project_ids(&self) -> impl Future<Output = Vec<ProjectId>> + Send;

    fn project_review_discovery_enabled(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<bool>> + Send;

    fn run_project_review_discovery(
        &self,
        project_id: ProjectId,
        cancellation_token: CancellationToken,
    ) -> impl Future<Output = Result<ProjectReviewSelectorRunResult>> + Send;

    fn project_review_discovery_snapshot(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<ProjectReviewDiscoverySnapshot>> + Send;

    fn update_project_review_discovery_snapshot(
        &self,
        project_id: ProjectId,
        snapshot: ProjectReviewDiscoverySnapshot,
    ) -> impl Future<Output = Result<()>> + Send;
}

pub(crate) struct ProjectReviewDiscoveryScheduler {
    cancellation_token: CancellationToken,
    notify: Arc<Notify>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ProjectReviewDiscoveryScheduler {
    pub(crate) fn new() -> Self {
        Self {
            cancellation_token: CancellationToken::new(),
            notify: Arc::new(Notify::new()),
            task: Mutex::new(None),
        }
    }

    pub(crate) async fn start<Ops>(&self, ops: Ops)
    where
        Ops: ProjectReviewDiscoveryOps,
    {
        let mut task = self.task.lock().await;
        if task.is_some() {
            return;
        }
        let cancellation_token = self.cancellation_token.clone();
        let notify = self.notify.clone();
        *task = Some(tokio::spawn(async move {
            run_project_review_discovery_scheduler(ops, cancellation_token, notify).await;
        }));
    }

    pub(crate) fn notify(&self) {
        self.notify.notify_one();
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.cancellation_token.cancel();
        self.notify.notify_waiters();
        let Some(task) = self.task.lock().await.take() else {
            return Ok(());
        };
        task.await.map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "project review discovery scheduler stopped unexpectedly: {error}"
            ))
        })
    }
}

impl Default for ProjectReviewDiscoveryScheduler {
    fn default() -> Self {
        Self::new()
    }
}

async fn run_project_review_discovery_scheduler<Ops>(
    ops: Ops,
    cancellation_token: CancellationToken,
    notify: Arc<Notify>,
) where
    Ops: ProjectReviewDiscoveryOps,
{
    let mut backoffs = HashMap::<ProjectId, ProjectReviewRetryBackoff>::new();
    let mut deadlines = HashMap::<ProjectId, Instant>::new();
    loop {
        if cancellation_token.is_cancelled() {
            break;
        }
        let due_projects = due_discovery_projects(&ops, &mut deadlines, &mut backoffs).await;
        let results = stream::iter(due_projects)
            .map(|project_id| {
                let ops = ops.clone();
                let cancellation_token = cancellation_token.clone();
                async move {
                    (
                        project_id,
                        run_project_discovery(&ops, project_id, cancellation_token).await,
                    )
                }
            })
            .buffer_unordered(DISCOVERY_PROJECT_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        if cancellation_token.is_cancelled() {
            break;
        }
        for (project_id, result) in results {
            let delay = finish_project_discovery(&ops, &mut backoffs, project_id, result).await;
            deadlines.insert(project_id, Instant::now() + delay);
        }
        match next_discovery_wake(&deadlines) {
            Some(wake_at) => {
                tokio::select! {
                    _ = cancellation_token.cancelled() => break,
                    _ = notify.notified() => {}
                    _ = sleep_until(wake_at) => {}
                }
            }
            None => {
                tokio::select! {
                    _ = cancellation_token.cancelled() => break,
                    _ = notify.notified() => {}
                }
            }
        }
    }
}

async fn due_discovery_projects(
    ops: &impl ProjectReviewDiscoveryOps,
    deadlines: &mut HashMap<ProjectId, Instant>,
    backoffs: &mut HashMap<ProjectId, ProjectReviewRetryBackoff>,
) -> Vec<ProjectId> {
    let project_ids = ops.project_review_discovery_project_ids().await;
    let current_projects = project_ids.iter().copied().collect::<HashSet<_>>();
    deadlines.retain(|project_id, _| current_projects.contains(project_id));
    backoffs.retain(|project_id, _| current_projects.contains(project_id));
    let now = Instant::now();
    let mut due = Vec::new();
    for project_id in project_ids {
        let enabled = match ops.project_review_discovery_enabled(project_id).await {
            Ok(enabled) => enabled,
            Err(error) => {
                tracing::warn!(project_id = %project_id, "failed to inspect review discovery project: {error}");
                deadlines.remove(&project_id);
                due.push(project_id);
                continue;
            }
        };
        if !enabled {
            deadlines.remove(&project_id);
            backoffs.remove(&project_id);
            disable_project_discovery(ops, project_id).await;
            continue;
        }
        if deadlines
            .get(&project_id)
            .is_none_or(|deadline| *deadline <= now)
        {
            due.push(project_id);
        }
    }
    due
}

async fn disable_project_discovery(ops: &impl ProjectReviewDiscoveryOps, project_id: ProjectId) {
    let Ok(mut snapshot) = ops.project_review_discovery_snapshot(project_id).await else {
        return;
    };
    if snapshot.state == ProjectReviewDiscoveryState::Disabled {
        return;
    }
    snapshot.state = ProjectReviewDiscoveryState::Disabled;
    snapshot.next_scan_at = None;
    snapshot.last_error = None;
    let _ = ops
        .update_project_review_discovery_snapshot(project_id, snapshot)
        .await;
}

struct ProjectDiscoveryRunResult {
    started_at: DateTime<Utc>,
    started_instant: Instant,
    result: Result<ProjectReviewSelectorRunResult>,
}

async fn run_project_discovery(
    ops: &impl ProjectReviewDiscoveryOps,
    project_id: ProjectId,
    cancellation_token: CancellationToken,
) -> ProjectDiscoveryRunResult {
    let started_at = Utc::now();
    let started_instant = Instant::now();
    let previous = ops
        .project_review_discovery_snapshot(project_id)
        .await
        .unwrap_or_default();
    let _ = ops
        .update_project_review_discovery_snapshot(
            project_id,
            ProjectReviewDiscoverySnapshot {
                state: ProjectReviewDiscoveryState::Scanning,
                last_started_at: Some(started_at),
                next_scan_at: None,
                last_error: None,
                ..previous
            },
        )
        .await;
    let result = ops
        .run_project_review_discovery(project_id, cancellation_token)
        .await;
    ProjectDiscoveryRunResult {
        started_at,
        started_instant,
        result,
    }
}

async fn finish_project_discovery(
    ops: &impl ProjectReviewDiscoveryOps,
    backoffs: &mut HashMap<ProjectId, ProjectReviewRetryBackoff>,
    project_id: ProjectId,
    run: ProjectDiscoveryRunResult,
) -> Duration {
    let completed_at = Utc::now();
    let previous = ops
        .project_review_discovery_snapshot(project_id)
        .await
        .unwrap_or_default();
    let (snapshot, delay) = match run.result {
        Ok(result) => {
            backoffs.remove(&project_id);
            let interval = Duration::from_secs(super::PROJECT_REVIEW_DISCOVERY_INTERVAL_SECS);
            let elapsed = run.started_instant.elapsed();
            let delay = if elapsed >= interval {
                interval
            } else {
                interval - elapsed
            };
            let next_scan_at = completed_at
                + TimeDelta::milliseconds(delay.as_millis().try_into().unwrap_or(i64::MAX));
            (
                ProjectReviewDiscoverySnapshot {
                    state: if result.errors.is_empty() {
                        ProjectReviewDiscoveryState::Idle
                    } else {
                        ProjectReviewDiscoveryState::Partial
                    },
                    last_started_at: Some(run.started_at),
                    last_completed_at: Some(completed_at),
                    next_scan_at: Some(next_scan_at),
                    counts: result.counts,
                    last_error: (!result.errors.is_empty()).then(|| result.errors.join("\n")),
                },
                delay,
            )
        }
        Err(error) => {
            let backoff = backoffs
                .entry(project_id)
                .or_insert_with(super::project_review_retry_backoff);
            let delay = backoff.next_delay();
            (
                ProjectReviewDiscoverySnapshot {
                    state: ProjectReviewDiscoveryState::Backoff,
                    last_started_at: Some(run.started_at),
                    last_completed_at: Some(completed_at),
                    next_scan_at: Some(
                        completed_at
                            + TimeDelta::milliseconds(
                                delay.as_millis().try_into().unwrap_or(i64::MAX),
                            ),
                    ),
                    last_error: Some(error.to_string()),
                    ..previous
                },
                delay,
            )
        }
    };
    if let Err(error) = ops
        .update_project_review_discovery_snapshot(project_id, snapshot)
        .await
    {
        tracing::warn!(project_id = %project_id, "failed to publish review discovery state: {error}");
    }
    delay
}

fn next_discovery_wake(deadlines: &HashMap<ProjectId, Instant>) -> Option<Instant> {
    deadlines.values().copied().min()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use mai_protocol::ProjectReviewDiscoveryCounts;
    use pretty_assertions::assert_eq;

    use super::*;

    #[derive(Default)]
    struct FakeDiscoveryState {
        projects: Mutex<Vec<ProjectId>>,
        enabled: Mutex<HashMap<ProjectId, bool>>,
        snapshots: Mutex<HashMap<ProjectId, ProjectReviewDiscoverySnapshot>>,
        calls: Mutex<HashMap<ProjectId, usize>>,
        failures: Mutex<HashSet<ProjectId>>,
        block: AtomicBool,
        released: AtomicBool,
        active: AtomicUsize,
        max_active: AtomicUsize,
        started: Notify,
        release: Notify,
    }

    impl FakeDiscoveryState {
        async fn with_projects(projects: Vec<ProjectId>) -> Arc<Self> {
            let state = Arc::new(Self::default());
            *state.projects.lock().await = projects.clone();
            let mut enabled = state.enabled.lock().await;
            let mut snapshots = state.snapshots.lock().await;
            for project_id in projects {
                enabled.insert(project_id, true);
                snapshots.insert(project_id, ProjectReviewDiscoverySnapshot::default());
            }
            drop(snapshots);
            drop(enabled);
            state
        }

        async fn call_count(&self, project_id: ProjectId) -> usize {
            self.calls
                .lock()
                .await
                .get(&project_id)
                .copied()
                .unwrap_or_default()
        }

        async fn wait_for_calls(&self, expected: usize) {
            loop {
                let calls = self.calls.lock().await.values().sum::<usize>();
                if calls >= expected {
                    return;
                }
                self.started.notified().await;
            }
        }
    }

    impl ProjectReviewDiscoveryOps for Arc<FakeDiscoveryState> {
        async fn project_review_discovery_project_ids(&self) -> Vec<ProjectId> {
            self.projects.lock().await.clone()
        }

        async fn project_review_discovery_enabled(&self, project_id: ProjectId) -> Result<bool> {
            Ok(self
                .enabled
                .lock()
                .await
                .get(&project_id)
                .copied()
                .unwrap_or_default())
        }

        async fn run_project_review_discovery(
            &self,
            project_id: ProjectId,
            cancellation_token: CancellationToken,
        ) -> Result<ProjectReviewSelectorRunResult> {
            *self.calls.lock().await.entry(project_id).or_default() += 1;
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            self.started.notify_waiters();
            if self.block.load(Ordering::SeqCst) && !self.released.load(Ordering::SeqCst) {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        self.active.fetch_sub(1, Ordering::SeqCst);
                        return Err(RuntimeError::TurnCancelled);
                    }
                    _ = self.release.notified() => {}
                }
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            if self.failures.lock().await.contains(&project_id) {
                return Err(RuntimeError::InvalidInput("selector failed".to_string()));
            }
            Ok(ProjectReviewSelectorRunResult {
                counts: ProjectReviewDiscoveryCounts {
                    scanned: 1,
                    ..Default::default()
                },
                errors: Vec::new(),
            })
        }

        async fn project_review_discovery_snapshot(
            &self,
            project_id: ProjectId,
        ) -> Result<ProjectReviewDiscoverySnapshot> {
            self.snapshots
                .lock()
                .await
                .get(&project_id)
                .cloned()
                .ok_or(RuntimeError::ProjectNotFound(project_id))
        }

        async fn update_project_review_discovery_snapshot(
            &self,
            project_id: ProjectId,
            snapshot: ProjectReviewDiscoverySnapshot,
        ) -> Result<()> {
            self.snapshots.lock().await.insert(project_id, snapshot);
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn starts_immediately_and_runs_on_the_fixed_ten_minute_deadline() {
        let project_id = ProjectId::new_v4();
        let ops = FakeDiscoveryState::with_projects(vec![project_id]).await;
        let scheduler = ProjectReviewDiscoveryScheduler::new();
        scheduler.start(ops.clone()).await;

        ops.wait_for_calls(1).await;
        let snapshot = ops.snapshots.lock().await[&project_id].clone();
        assert_eq!(ProjectReviewDiscoveryState::Idle, snapshot.state);
        assert_eq!(1, snapshot.counts.scanned);
        let scheduled_after = snapshot
            .next_scan_at
            .zip(snapshot.last_completed_at)
            .map(|(next, completed)| (next - completed).num_seconds());
        assert_eq!(Some(600), scheduled_after);

        tokio::time::advance(Duration::from_secs(599)).await;
        tokio::task::yield_now().await;
        assert_eq!(1, ops.call_count(project_id).await);
        tokio::time::advance(Duration::from_secs(1)).await;
        ops.wait_for_calls(2).await;

        scheduler.shutdown().await.expect("shutdown scheduler");
    }

    #[tokio::test(start_paused = true)]
    async fn notify_scans_a_project_that_has_just_become_enabled() {
        let project_id = ProjectId::new_v4();
        let ops = FakeDiscoveryState::with_projects(vec![project_id]).await;
        ops.enabled.lock().await.insert(project_id, false);
        let scheduler = ProjectReviewDiscoveryScheduler::new();
        scheduler.start(ops.clone()).await;
        tokio::task::yield_now().await;
        assert_eq!(0, ops.call_count(project_id).await);
        assert_eq!(
            ProjectReviewDiscoveryState::Disabled,
            ops.snapshots.lock().await[&project_id].state
        );

        ops.enabled.lock().await.insert(project_id, true);
        scheduler.notify();
        ops.wait_for_calls(1).await;

        scheduler.shutdown().await.expect("shutdown scheduler");
    }

    #[tokio::test(start_paused = true)]
    async fn isolates_project_failures_and_limits_project_concurrency() {
        let projects = (0..3).map(|_| ProjectId::new_v4()).collect::<Vec<_>>();
        let ops = FakeDiscoveryState::with_projects(projects.clone()).await;
        ops.block.store(true, Ordering::SeqCst);
        ops.failures.lock().await.insert(projects[0]);
        let scheduler = ProjectReviewDiscoveryScheduler::new();
        scheduler.start(ops.clone()).await;
        ops.wait_for_calls(2).await;
        assert_eq!(2, ops.max_active.load(Ordering::SeqCst));

        scheduler.notify();
        tokio::task::yield_now().await;
        assert_eq!(2, ops.calls.lock().await.values().sum::<usize>());
        ops.released.store(true, Ordering::SeqCst);
        ops.release.notify_waiters();
        ops.wait_for_calls(3).await;
        loop {
            let snapshots = ops.snapshots.lock().await;
            if snapshots.values().all(|snapshot| {
                matches!(
                    snapshot.state,
                    ProjectReviewDiscoveryState::Idle | ProjectReviewDiscoveryState::Backoff
                )
            }) {
                break;
            }
            drop(snapshots);
            tokio::task::yield_now().await;
        }
        let snapshots = ops.snapshots.lock().await;
        assert_eq!(
            ProjectReviewDiscoveryState::Backoff,
            snapshots[&projects[0]].state
        );
        assert_eq!(
            ProjectReviewDiscoveryState::Idle,
            snapshots[&projects[1]].state
        );
        assert_eq!(
            ProjectReviewDiscoveryState::Idle,
            snapshots[&projects[2]].state
        );
        drop(snapshots);

        scheduler.shutdown().await.expect("shutdown scheduler");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_and_drains_an_active_scan() {
        let project_id = ProjectId::new_v4();
        let ops = FakeDiscoveryState::with_projects(vec![project_id]).await;
        ops.block.store(true, Ordering::SeqCst);
        let scheduler = ProjectReviewDiscoveryScheduler::new();
        scheduler.start(ops.clone()).await;
        ops.wait_for_calls(1).await;

        scheduler.shutdown().await.expect("shutdown scheduler");

        assert_eq!(0, ops.active.load(Ordering::SeqCst));
        assert_eq!(1, ops.call_count(project_id).await);
    }
}
