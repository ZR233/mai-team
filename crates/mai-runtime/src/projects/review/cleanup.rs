use std::future::Future;

use chrono::{DateTime, TimeDelta, Utc};
use mai_store::ProjectReviewCleanupTask;
use tokio::time::{Duration, sleep};
use uuid::Uuid;

use crate::{MaiRetentionConfig, Result};

pub(crate) const PROJECT_REVIEW_PRODUCT_EVENT_LIMIT: usize = 50_000;
const RESOURCE_CLEANUP_INTERVAL_SECS: u64 = 60;
const RESOURCE_CLEANUP_LEASE_SECS: i64 = 5 * 60;
const RESOURCE_CLEANUP_BATCH_SIZE: usize = 500;
const RESOURCE_CLEANUP_TIMEOUT_SECS: u64 = 2 * 60;

/// Supplies persistence, event, and workspace side effects for review retention cleanup.
pub(crate) trait ProjectReviewCleanupOps: Send + Sync {
    fn retention_config(&self) -> impl Future<Output = MaiRetentionConfig> + Send;

    fn prune_project_review_jobs_before(
        &self,
        cutoff: DateTime<Utc>,
        batch_size: usize,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn prune_orphan_project_review_runs_before(
        &self,
        cutoff: DateTime<Utc>,
        batch_size: usize,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn prune_product_events_before(
        &self,
        cutoff: DateTime<Utc>,
        batch_size: usize,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn prune_product_events_to_limit(
        &self,
        limit: usize,
        batch_size: usize,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn prune_agent_logs_before(
        &self,
        cutoff: DateTime<Utc>,
        batch_size: usize,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn prune_tool_traces_before(
        &self,
        cutoff: DateTime<Utc>,
        batch_size: usize,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn cleanup_tool_output_namespaces(
        &self,
        cutoff: std::time::SystemTime,
        batch_size: usize,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn retain_events_since(&self, cutoff: DateTime<Utc>) -> impl Future<Output = ()> + Send;

    fn reconcile_project_volumes(&self) -> impl Future<Output = Result<()>> + Send;

    fn claim_due_project_review_cleanup_task(
        &self,
        owner: String,
        now: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> impl Future<Output = Result<Option<ProjectReviewCleanupTask>>> + Send;

    fn execute_project_review_cleanup_task(
        &self,
        task: ProjectReviewCleanupTask,
    ) -> impl Future<Output = Result<()>> + Send;

    fn complete_project_review_cleanup_task(
        &self,
        task_id: String,
        owner: String,
        finished_at: DateTime<Utc>,
    ) -> impl Future<Output = Result<bool>> + Send;

    fn retry_project_review_cleanup_task(
        &self,
        task_id: String,
        owner: String,
        next_attempt_at: DateTime<Utc>,
        error: String,
    ) -> impl Future<Output = Result<bool>> + Send;
}

pub(crate) async fn run_project_review_cleanup_loop(ops: &impl ProjectReviewCleanupOps) {
    loop {
        let interval = ops.retention_config().await.cleanup_interval_secs;
        sleep(Duration::from_secs(interval)).await;
        if let Err(err) = cleanup_project_review_history(ops).await {
            tracing::warn!("project review cleanup failed: {err}");
        }
    }
}

pub(crate) async fn run_project_review_resource_cleanup_loop(ops: &impl ProjectReviewCleanupOps) {
    let owner = format!("review-cleanup-{}", Uuid::new_v4());
    loop {
        if let Err(error) = cleanup_due_project_review_resources(ops, &owner).await {
            tracing::warn!(%error, "project review resource cleanup failed");
        }
        sleep(Duration::from_secs(RESOURCE_CLEANUP_INTERVAL_SECS)).await;
    }
}

async fn cleanup_due_project_review_resources(
    ops: &impl ProjectReviewCleanupOps,
    owner: &str,
) -> Result<usize> {
    let started_at = std::time::Instant::now();
    let mut completed = 0;
    let mut retried = 0;
    for _ in 0..RESOURCE_CLEANUP_BATCH_SIZE {
        let now = Utc::now();
        let task = ops
            .claim_due_project_review_cleanup_task(
                owner.to_string(),
                now,
                now + TimeDelta::seconds(RESOURCE_CLEANUP_LEASE_SECS),
            )
            .await?;
        let Some(task) = task else {
            break;
        };
        let result = tokio::time::timeout(
            Duration::from_secs(RESOURCE_CLEANUP_TIMEOUT_SECS),
            ops.execute_project_review_cleanup_task(task.clone()),
        )
        .await;
        match result {
            Ok(Ok(())) => {
                if ops
                    .complete_project_review_cleanup_task(
                        task.id.clone(),
                        owner.to_string(),
                        Utc::now(),
                    )
                    .await?
                {
                    completed += 1;
                }
            }
            Ok(Err(error)) => {
                schedule_resource_cleanup_retry(ops, owner, &task, error.to_string()).await?;
                retried += 1;
            }
            Err(_) => {
                schedule_resource_cleanup_retry(
                    ops,
                    owner,
                    &task,
                    format!("cleanup exceeded {} seconds", RESOURCE_CLEANUP_TIMEOUT_SECS),
                )
                .await?;
                retried += 1;
            }
        }
    }
    if completed > 0 || retried > 0 {
        tracing::info!(
            completed,
            retried,
            elapsed_ms = started_at.elapsed().as_millis(),
            "processed project review resource cleanup batch"
        );
    }
    Ok(completed)
}

async fn schedule_resource_cleanup_retry(
    ops: &impl ProjectReviewCleanupOps,
    owner: &str,
    task: &ProjectReviewCleanupTask,
    error: String,
) -> Result<()> {
    let next_attempt_at =
        Utc::now() + TimeDelta::seconds(resource_cleanup_backoff_secs(task.attempt_count));
    let updated = ops
        .retry_project_review_cleanup_task(
            task.id.clone(),
            owner.to_string(),
            next_attempt_at,
            error.clone(),
        )
        .await?;
    if updated {
        tracing::warn!(
            task_id = %task.id,
            resource_kind = %task.resource_kind,
            resource_id = %task.resource_id,
            attempt_count = task.attempt_count,
            next_attempt_at = %next_attempt_at,
            %error,
            "project review resource cleanup scheduled for retry"
        );
    }
    Ok(())
}

fn resource_cleanup_backoff_secs(attempt_count: u32) -> i64 {
    match attempt_count {
        0 | 1 => 5,
        2 => 30,
        3 => 2 * 60,
        4 => 10 * 60,
        _ => 60 * 60,
    }
}

pub(crate) async fn cleanup_project_review_history(
    ops: &impl ProjectReviewCleanupOps,
) -> Result<()> {
    let started_at = std::time::Instant::now();
    let now = Utc::now();
    let retention = ops.retention_config().await;
    let batch_size = retention.cleanup_batch_size;
    let review_cutoff = now - TimeDelta::days(retention.review_history_days);
    let mut removed_jobs = 0;
    loop {
        let removed = ops
            .prune_project_review_jobs_before(review_cutoff, batch_size)
            .await?;
        removed_jobs += removed;
        if removed < batch_size {
            break;
        }
        tokio::task::yield_now().await;
    }
    let mut removed_orphan_runs = 0;
    loop {
        let removed = ops
            .prune_orphan_project_review_runs_before(review_cutoff, batch_size)
            .await?;
        removed_orphan_runs += removed;
        if removed < batch_size {
            break;
        }
        tokio::task::yield_now().await;
    }
    let product_event_cutoff = now - TimeDelta::days(retention.product_events_days);
    let removed_events = ops
        .prune_product_events_before(product_event_cutoff, batch_size)
        .await?;
    let removed_events_by_limit = ops
        .prune_product_events_to_limit(PROJECT_REVIEW_PRODUCT_EVENT_LIMIT, batch_size)
        .await?;
    let removed_logs = ops
        .prune_agent_logs_before(now - TimeDelta::days(retention.agent_logs_days), batch_size)
        .await?;
    let removed_traces = ops
        .prune_tool_traces_before(
            now - TimeDelta::days(retention.tool_traces_days),
            batch_size,
        )
        .await?;
    let tool_output_cutoff: std::time::SystemTime =
        (now - TimeDelta::days(retention.tool_output_days)).into();
    let mut removed_tool_outputs = 0;
    loop {
        let batch_started_at = std::time::Instant::now();
        let removed = ops
            .cleanup_tool_output_namespaces(tool_output_cutoff, batch_size)
            .await?;
        removed_tool_outputs += removed;
        if removed > 0 {
            tracing::info!(
                removed,
                elapsed_ms = batch_started_at.elapsed().as_millis(),
                "pruned tool-output retention batch"
            );
        }
        if removed < batch_size {
            break;
        }
        tokio::task::yield_now().await;
    }
    if removed_jobs > 0
        || removed_orphan_runs > 0
        || removed_events > 0
        || removed_events_by_limit > 0
        || removed_logs > 0
        || removed_traces > 0
        || removed_tool_outputs > 0
    {
        tracing::info!(
            removed_jobs,
            removed_orphan_runs,
            removed_events,
            removed_events_by_limit,
            removed_logs,
            removed_traces,
            removed_tool_outputs,
            elapsed_ms = started_at.elapsed().as_millis(),
            "pruned project review history"
        );
    }
    ops.retain_events_since(product_event_cutoff).await;
    ops.reconcile_project_volumes().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{DateTime, Utc};
    use tokio::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakeCleanupOps {
        product_event_limits: Arc<Mutex<Vec<usize>>>,
        review_cutoffs: Arc<Mutex<Vec<DateTime<Utc>>>>,
        review_job_batches: Arc<Mutex<Vec<usize>>>,
        orphan_run_batches: Arc<Mutex<Vec<usize>>>,
    }

    impl ProjectReviewCleanupOps for FakeCleanupOps {
        async fn retention_config(&self) -> MaiRetentionConfig {
            MaiRetentionConfig::default()
        }

        async fn prune_project_review_jobs_before(
            &self,
            cutoff: DateTime<Utc>,
            _batch_size: usize,
        ) -> Result<usize> {
            self.review_cutoffs.lock().await.push(cutoff);
            let mut batches = self.review_job_batches.lock().await;
            Ok(if batches.is_empty() {
                0
            } else {
                batches.remove(0)
            })
        }

        async fn prune_orphan_project_review_runs_before(
            &self,
            cutoff: DateTime<Utc>,
            _batch_size: usize,
        ) -> Result<usize> {
            self.review_cutoffs.lock().await.push(cutoff);
            let mut batches = self.orphan_run_batches.lock().await;
            Ok(if batches.is_empty() {
                0
            } else {
                batches.remove(0)
            })
        }

        async fn prune_product_events_before(
            &self,
            _cutoff: DateTime<Utc>,
            _batch_size: usize,
        ) -> Result<usize> {
            Ok(0)
        }

        async fn prune_product_events_to_limit(
            &self,
            limit: usize,
            _batch_size: usize,
        ) -> Result<usize> {
            self.product_event_limits.lock().await.push(limit);
            Ok(2)
        }

        async fn prune_agent_logs_before(
            &self,
            _cutoff: DateTime<Utc>,
            _batch_size: usize,
        ) -> Result<usize> {
            Ok(0)
        }

        async fn prune_tool_traces_before(
            &self,
            _cutoff: DateTime<Utc>,
            _batch_size: usize,
        ) -> Result<usize> {
            Ok(0)
        }

        async fn cleanup_tool_output_namespaces(
            &self,
            _cutoff: std::time::SystemTime,
            _batch_size: usize,
        ) -> Result<usize> {
            Ok(0)
        }
        async fn retain_events_since(&self, _cutoff: DateTime<Utc>) {}

        async fn reconcile_project_volumes(&self) -> Result<()> {
            Ok(())
        }

        async fn claim_due_project_review_cleanup_task(
            &self,
            _owner: String,
            _now: DateTime<Utc>,
            _lease_expires_at: DateTime<Utc>,
        ) -> Result<Option<ProjectReviewCleanupTask>> {
            Ok(None)
        }

        async fn execute_project_review_cleanup_task(
            &self,
            _task: ProjectReviewCleanupTask,
        ) -> Result<()> {
            Ok(())
        }

        async fn complete_project_review_cleanup_task(
            &self,
            _task_id: String,
            _owner: String,
            _finished_at: DateTime<Utc>,
        ) -> Result<bool> {
            Ok(true)
        }

        async fn retry_project_review_cleanup_task(
            &self,
            _task_id: String,
            _owner: String,
            _next_attempt_at: DateTime<Utc>,
            _error: String,
        ) -> Result<bool> {
            Ok(true)
        }
    }

    #[tokio::test]
    async fn cleanup_caps_persisted_product_events() {
        let ops = FakeCleanupOps::default();

        cleanup_project_review_history(&ops).await.expect("cleanup");

        assert_eq!(
            *ops.product_event_limits.lock().await,
            vec![PROJECT_REVIEW_PRODUCT_EVENT_LIMIT]
        );
        let review_cutoffs = ops.review_cutoffs.lock().await;
        assert_eq!(review_cutoffs.len(), 2);
        assert_eq!(review_cutoffs[0], review_cutoffs[1]);
    }

    #[tokio::test]
    async fn cleanup_drains_review_history_batches_in_one_cycle() {
        let ops = FakeCleanupOps::default();
        *ops.review_job_batches.lock().await = vec![500, 500, 13];
        *ops.orphan_run_batches.lock().await = vec![500, 2];

        cleanup_project_review_history(&ops).await.expect("cleanup");

        assert_eq!(5, ops.review_cutoffs.lock().await.len());
        assert!(ops.review_job_batches.lock().await.is_empty());
        assert!(ops.orphan_run_batches.lock().await.is_empty());
    }

    #[test]
    fn resource_cleanup_backoff_is_bounded_at_one_hour() {
        assert_eq!(5, resource_cleanup_backoff_secs(1));
        assert_eq!(30, resource_cleanup_backoff_secs(2));
        assert_eq!(120, resource_cleanup_backoff_secs(3));
        assert_eq!(600, resource_cleanup_backoff_secs(4));
        assert_eq!(3600, resource_cleanup_backoff_secs(5));
        assert_eq!(3600, resource_cleanup_backoff_secs(50));
    }
}
