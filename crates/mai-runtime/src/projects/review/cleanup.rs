use std::future::Future;

use chrono::{DateTime, TimeDelta, Utc};
use mai_protocol::ProjectSummary;
use tokio::time::{Duration, sleep};

use crate::Result;

pub(crate) const PROJECT_REVIEW_HISTORY_RETENTION_DAYS: i64 = 5;
pub(crate) const PROJECT_REVIEW_CLEANUP_INTERVAL_SECS: u64 = 3600;

/// Supplies persistence, event, and workspace side effects for review retention cleanup.
pub(crate) trait ProjectReviewCleanupOps: Send + Sync {
    fn prune_project_review_runs_before(
        &self,
        cutoff: DateTime<Utc>,
    ) -> impl Future<Output = Result<usize>> + Send;

    fn prune_service_events_before(
        &self,
        cutoff: DateTime<Utc>,
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
    let removed_events = ops.prune_service_events_before(cutoff).await?;
    let removed_logs = ops.prune_agent_logs_before(cutoff).await?;
    let removed_traces = ops.prune_tool_traces_before(cutoff).await?;
    if removed_runs > 0 || removed_events > 0 || removed_logs > 0 || removed_traces > 0 {
        tracing::info!(
            removed_runs,
            removed_events,
            removed_logs,
            removed_traces,
            "pruned project review history"
        );
    }
    ops.retain_events_since(cutoff).await;
    let _ = ops.list_projects().await;
    Ok(())
}
