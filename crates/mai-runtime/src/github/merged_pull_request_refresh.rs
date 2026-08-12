use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use mai_protocol::{ProjectId, ProjectPullRequestMergeRefreshSummary};
use reqwest::StatusCode;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use crate::{Result, RuntimeError};

const GLOBAL_QUERY_CONCURRENCY: usize = 4;

#[derive(Default)]
struct RefreshState {
    running: bool,
    generation: u64,
    result: Option<(u64, SharedRefreshResult)>,
}

struct RefreshEntry {
    state: Mutex<RefreshState>,
    notify: Notify,
}

impl RefreshEntry {
    fn new() -> Self {
        Self {
            state: Mutex::new(RefreshState::default()),
            notify: Notify::new(),
        }
    }
}

#[derive(Clone)]
enum SharedRefreshError {
    GithubUnavailable {
        operation: String,
        status: StatusCode,
        message: String,
        retry_after: Option<std::time::Duration>,
    },
    InvalidInput(String),
    Internal(String),
}

type SharedRefreshResult =
    std::result::Result<ProjectPullRequestMergeRefreshSummary, SharedRefreshError>;

enum RefreshRole {
    Leader { generation: u64 },
    Follower { generation: u64 },
}

/// 协调同一项目的 merged 状态刷新，并限制跨项目的 GitHub 查询并发。
///
/// 每个项目同时只有一个 leader；完成或取消时由 guard 原子发布结果、移除
/// flight 并唤醒所有 follower，防止请求取消后留下永久等待的订阅者。
pub(crate) struct MergedPullRequestRefreshCoordinator {
    entries: Mutex<HashMap<ProjectId, Arc<RefreshEntry>>>,
    github_queries: Arc<Semaphore>,
}

impl Default for MergedPullRequestRefreshCoordinator {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            github_queries: Arc::new(Semaphore::new(GLOBAL_QUERY_CONCURRENCY)),
        }
    }
}

impl MergedPullRequestRefreshCoordinator {
    pub(crate) async fn run<Operation, OperationFuture>(
        &self,
        project_id: ProjectId,
        operation: Operation,
    ) -> Result<ProjectPullRequestMergeRefreshSummary>
    where
        Operation: FnOnce() -> OperationFuture,
        OperationFuture: Future<Output = Result<ProjectPullRequestMergeRefreshSummary>>,
    {
        let entry = {
            let mut entries = lock(&self.entries);
            Arc::clone(
                entries
                    .entry(project_id)
                    .or_insert_with(|| Arc::new(RefreshEntry::new())),
            )
        };
        let role = {
            let mut state = lock(&entry.state);
            if state.running {
                RefreshRole::Follower {
                    generation: state.generation,
                }
            } else {
                state.running = true;
                state.result = None;
                RefreshRole::Leader {
                    generation: state.generation.saturating_add(1),
                }
            }
        };
        let generation = match role {
            RefreshRole::Leader { generation } => generation,
            RefreshRole::Follower { generation } => {
                return Self::wait_for_result(&entry, generation).await;
            }
        };

        let mut guard = RefreshLeaderGuard::new(self, project_id, Arc::clone(&entry), generation);
        let result = operation().await.map_err(SharedRefreshError::from);
        guard.complete(result.clone());
        shared_result(result)
    }

    async fn wait_for_result(
        entry: &RefreshEntry,
        generation: u64,
    ) -> Result<ProjectPullRequestMergeRefreshSummary> {
        loop {
            let notified = entry.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = lock(&entry.state);
                if let Some((completed_generation, result)) = &state.result
                    && *completed_generation > generation
                {
                    return shared_result(result.clone());
                }
            }
            notified.await;
        }
    }

    pub(crate) async fn acquire_github_query(&self) -> Result<OwnedSemaphorePermit> {
        Arc::clone(&self.github_queries)
            .acquire_owned()
            .await
            .map_err(|error| RuntimeError::MergedPullRequestRefresh(error.to_string()))
    }

    fn finish(
        &self,
        project_id: ProjectId,
        entry: &Arc<RefreshEntry>,
        generation: u64,
        result: SharedRefreshResult,
    ) {
        let mut entries = lock(&self.entries);
        let mut state = lock(&entry.state);
        state.running = false;
        state.generation = generation;
        state.result = Some((generation, result));
        if entries
            .get(&project_id)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            entries.remove(&project_id);
        }
        drop(state);
        drop(entries);
        entry.notify.notify_waiters();
    }
}

struct RefreshLeaderGuard<'a> {
    coordinator: &'a MergedPullRequestRefreshCoordinator,
    project_id: ProjectId,
    entry: Arc<RefreshEntry>,
    generation: u64,
    completed: bool,
}

impl<'a> RefreshLeaderGuard<'a> {
    fn new(
        coordinator: &'a MergedPullRequestRefreshCoordinator,
        project_id: ProjectId,
        entry: Arc<RefreshEntry>,
        generation: u64,
    ) -> Self {
        Self {
            coordinator,
            project_id,
            entry,
            generation,
            completed: false,
        }
    }

    fn complete(&mut self, result: SharedRefreshResult) {
        self.coordinator
            .finish(self.project_id, &self.entry, self.generation, result);
        self.completed = true;
    }
}

impl Drop for RefreshLeaderGuard<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.coordinator.finish(
            self.project_id,
            &self.entry,
            self.generation,
            Err(SharedRefreshError::Internal(
                "merged pull request refresh was cancelled".to_string(),
            )),
        );
    }
}

impl From<RuntimeError> for SharedRefreshError {
    fn from(error: RuntimeError) -> Self {
        match error {
            RuntimeError::GithubUnavailable {
                operation,
                status,
                message,
                retry_after,
            } => Self::GithubUnavailable {
                operation,
                status,
                message,
                retry_after,
            },
            RuntimeError::InvalidInput(message) => Self::InvalidInput(message),
            error => Self::Internal(error.to_string()),
        }
    }
}

fn shared_result(result: SharedRefreshResult) -> Result<ProjectPullRequestMergeRefreshSummary> {
    result.map_err(|error| match error {
        SharedRefreshError::GithubUnavailable {
            operation,
            status,
            message,
            retry_after,
        } => RuntimeError::GithubUnavailable {
            operation,
            status,
            message,
            retry_after,
        },
        SharedRefreshError::InvalidInput(message) => RuntimeError::InvalidInput(message),
        SharedRefreshError::Internal(message) => RuntimeError::MergedPullRequestRefresh(message),
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn concurrent_project_refreshes_share_the_same_execution() {
        let coordinator = Arc::new(MergedPullRequestRefreshCoordinator::default());
        let project_id = ProjectId::new_v4();
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());

        let first = spawn_refresh(
            Arc::clone(&coordinator),
            project_id,
            Arc::clone(&calls),
            Arc::clone(&started),
            Arc::clone(&release),
        );
        started.notified().await;
        let second = {
            let coordinator = Arc::clone(&coordinator);
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                coordinator
                    .run(project_id, || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(summary(99, 99))
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        release.notify_waiters();

        assert_eq!(
            first.await.expect("first task").expect("first result"),
            summary(3, 1)
        );
        assert_eq!(
            second.await.expect("second task").expect("second result"),
            summary(3, 1)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_leader_releases_followers_and_allows_retry() {
        let coordinator = Arc::new(MergedPullRequestRefreshCoordinator::default());
        let project_id = ProjectId::new_v4();
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let leader = spawn_refresh(
            Arc::clone(&coordinator),
            project_id,
            Arc::clone(&calls),
            Arc::clone(&started),
            release,
        );
        started.notified().await;
        let follower = {
            let coordinator = Arc::clone(&coordinator);
            tokio::spawn(async move {
                coordinator
                    .run(project_id, || async { Ok(summary(9, 9)) })
                    .await
            })
        };
        tokio::task::yield_now().await;
        leader.abort();

        let error = tokio::time::timeout(std::time::Duration::from_secs(1), follower)
            .await
            .expect("follower must be released")
            .expect("follower task")
            .expect_err("cancelled refresh must fail");
        assert!(matches!(error, RuntimeError::MergedPullRequestRefresh(_)));
        assert_eq!(
            coordinator
                .run(project_id, || async { Ok(summary(4, 2)) })
                .await
                .expect("retry"),
            summary(4, 2)
        );
    }

    #[tokio::test]
    async fn failed_execution_is_shared_and_next_request_retries() {
        let coordinator = Arc::new(MergedPullRequestRefreshCoordinator::default());
        let project_id = ProjectId::new_v4();
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let leader = {
            let coordinator = Arc::clone(&coordinator);
            let calls = Arc::clone(&calls);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                coordinator
                    .run(project_id, || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        started.notify_one();
                        release.notified().await;
                        Err(RuntimeError::InvalidInput("query failed".to_string()))
                    })
                    .await
            })
        };
        started.notified().await;
        let follower = {
            let coordinator = Arc::clone(&coordinator);
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                coordinator
                    .run(project_id, || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(summary(99, 99))
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        release.notify_waiters();

        for result in [
            leader.await.expect("leader"),
            follower.await.expect("follower"),
        ] {
            assert!(
                matches!(result, Err(RuntimeError::InvalidInput(message)) if message == "query failed")
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            coordinator
                .run(project_id, || async { Ok(summary(5, 0)) })
                .await
                .expect("retry"),
            summary(5, 0)
        );
    }

    fn spawn_refresh(
        coordinator: Arc<MergedPullRequestRefreshCoordinator>,
        project_id: ProjectId,
        calls: Arc<AtomicUsize>,
        started: Arc<Notify>,
        release: Arc<Notify>,
    ) -> tokio::task::JoinHandle<Result<ProjectPullRequestMergeRefreshSummary>> {
        tokio::spawn(async move {
            coordinator
                .run(project_id, || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    release.notified().await;
                    Ok(summary(3, 1))
                })
                .await
        })
    }

    fn summary(checked: usize, newly_merged: usize) -> ProjectPullRequestMergeRefreshSummary {
        ProjectPullRequestMergeRefreshSummary {
            checked,
            newly_merged,
        }
    }
}
