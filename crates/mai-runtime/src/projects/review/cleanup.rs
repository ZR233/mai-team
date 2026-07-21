use std::future::Future;

use chrono::{DateTime, TimeDelta, Utc};
use mai_protocol::ProjectSummary;
use tokio::time::{Duration, sleep};

use crate::Result;

pub(crate) const PROJECT_REVIEW_HISTORY_RETENTION_DAYS: i64 = 5;
pub(crate) const PROJECT_REVIEW_CLEANUP_INTERVAL_SECS: u64 = 3600;
pub(crate) const PROJECT_REVIEW_PRODUCT_EVENT_LIMIT: usize = 50_000;

/// Supplies persistence, event, and workspace side effects for review retention cleanup.
pub(crate) trait ProjectReviewCleanupOps: Send + Sync {
    fn prune_project_review_runs_before(
        &self,
        cutoff: DateTime<Utc>,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn prune_product_events_before(
        &self,
        cutoff: DateTime<Utc>,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn prune_product_events_to_limit(
        &self,
        limit: usize,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn prune_agent_logs_before(
        &self,
        cutoff: DateTime<Utc>,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn prune_tool_traces_before(
        &self,
        cutoff: DateTime<Utc>,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn retain_events_since(&self, cutoff: DateTime<Utc>) -> impl Future<Output = ()> + Send;

    fn list_projects(&self) -> impl Future<Output = Vec<ProjectSummary>> + Send;
}

pub(crate) async fn run_project_review_cleanup_loop(ops: &impl ProjectReviewCleanupOps) {
    if let Err(err) = cleanup_project_review_history(ops).await {
        tracing::warn!("project review cleanup failed: {err}");
    }
    loop {
        sleep(Duration::from_secs(PROJECT_REVIEW_CLEANUP_INTERVAL_SECS)).await;
        if let Err(err) = cleanup_project_review_history(ops).await {
            tracing::warn!("project review cleanup failed: {err}");
        }
    }
}

pub(crate) async fn cleanup_project_review_history(
    ops: &impl ProjectReviewCleanupOps,
) -> Result<()> {
    let cutoff = Utc::now() - TimeDelta::days(PROJECT_REVIEW_HISTORY_RETENTION_DAYS);
    let removed_runs = ops.prune_project_review_runs_before(cutoff).await?;
    let removed_events = ops.prune_product_events_before(cutoff).await?;
    let removed_events_by_limit = ops
        .prune_product_events_to_limit(PROJECT_REVIEW_PRODUCT_EVENT_LIMIT)
        .await?;
    let removed_logs = ops.prune_agent_logs_before(cutoff).await?;
    let removed_traces = ops.prune_tool_traces_before(cutoff).await?;
    if removed_runs > 0
        || removed_events > 0
        || removed_events_by_limit > 0
        || removed_logs > 0
        || removed_traces > 0
    {
        tracing::info!(
            removed_runs,
            removed_events,
            removed_events_by_limit,
            removed_logs,
            removed_traces,
            "pruned project review history"
        );
    }
    ops.retain_events_since(cutoff).await;
    let _ = ops.list_projects().await;
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
    }

    impl ProjectReviewCleanupOps for FakeCleanupOps {
        async fn prune_project_review_runs_before(&self, _cutoff: DateTime<Utc>) -> Result<usize> {
            Ok(0)
        }

        async fn prune_product_events_before(&self, _cutoff: DateTime<Utc>) -> Result<usize> {
            Ok(0)
        }

        async fn prune_product_events_to_limit(&self, limit: usize) -> Result<usize> {
            self.product_event_limits.lock().await.push(limit);
            Ok(2)
        }

        async fn prune_agent_logs_before(&self, _cutoff: DateTime<Utc>) -> Result<usize> {
            Ok(0)
        }

        async fn prune_tool_traces_before(&self, _cutoff: DateTime<Utc>) -> Result<usize> {
            Ok(0)
        }

        async fn retain_events_since(&self, _cutoff: DateTime<Utc>) {}

        async fn list_projects(&self) -> Vec<ProjectSummary> {
            Vec::new()
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
    }
}
