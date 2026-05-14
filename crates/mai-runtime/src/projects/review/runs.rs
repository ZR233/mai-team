use std::future::Future;

use chrono::{TimeDelta, Utc};
use mai_protocol::{
    AgentId, AgentMessage, ProjectId, ProjectReviewOutcome, ProjectReviewRunDetail,
    ProjectReviewRunStatus, ProjectReviewRunSummary, ProjectReviewRunsResponse, ServiceEvent,
    TurnId, now,
};
use mai_store::ConfigStore;
use uuid::Uuid;

use crate::{Result, RuntimeError};

/// Provides recent reviewer activity for review run snapshots without exposing
/// the runtime's full agent and event internals to review persistence.
pub(crate) trait ReviewRunSnapshotSource: Send + Sync {
    fn snapshot(
        &self,
        reviewer_agent_id: AgentId,
    ) -> impl Future<Output = (Vec<AgentMessage>, Vec<ServiceEvent>)> + Send;
}

#[derive(Debug, Clone)]
pub(crate) struct FinishReviewRun {
    pub(crate) run_id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) reviewer_agent_id: Option<AgentId>,
    pub(crate) turn_id: Option<TurnId>,
    pub(crate) status: ProjectReviewRunStatus,
    pub(crate) outcome: Option<ProjectReviewOutcome>,
    pub(crate) pr: Option<u64>,
    pub(crate) summary_text: Option<String>,
    pub(crate) error: Option<String>,
}

pub(crate) async fn list_project_review_runs(
    store: &ConfigStore,
    project_id: ProjectId,
    retention_days: i64,
    offset: usize,
    limit: usize,
) -> Result<ProjectReviewRunsResponse> {
    let since = Utc::now() - TimeDelta::days(retention_days);
    let runs = store
        .load_project_review_runs(project_id, Some(since), offset, limit)
        .await?;
    Ok(ProjectReviewRunsResponse { runs })
}

pub(crate) async fn get_project_review_run(
    store: &ConfigStore,
    project_id: ProjectId,
    run_id: Uuid,
) -> Result<ProjectReviewRunDetail> {
    store
        .load_project_review_run(project_id, run_id)
        .await?
        .ok_or(RuntimeError::ProjectReviewRunNotFound(run_id))
}

pub(crate) async fn record_project_review_startup_failure(
    store: &ConfigStore,
    project_id: ProjectId,
    error: String,
) -> Result<()> {
    let run_id = Uuid::new_v4();
    save_project_review_run_status(
        store,
        ProjectReviewRunSummary {
            id: run_id,
            project_id,
            reviewer_agent_id: None,
            turn_id: None,
            started_at: now(),
            finished_at: Some(now()),
            status: ProjectReviewRunStatus::Failed,
            outcome: Some(ProjectReviewOutcome::Failed),
            pr: None,
            summary: None,
            error: Some(error),
        },
        Vec::new(),
        Vec::new(),
    )
    .await
}

pub(crate) async fn cancel_active_project_review_runs(
    store: &ConfigStore,
    snapshot_source: &impl ReviewRunSnapshotSource,
    project_id: ProjectId,
    reviewer_agent_id: Option<AgentId>,
    run_list_limit: usize,
) -> Result<()> {
    let runs = store
        .load_project_review_runs(project_id, None, 0, run_list_limit)
        .await?;
    for run in runs {
        if run.finished_at.is_some()
            || !matches!(
                run.status,
                ProjectReviewRunStatus::Syncing | ProjectReviewRunStatus::Running
            )
            || reviewer_agent_id.is_some_and(|id| run.reviewer_agent_id != Some(id))
        {
            continue;
        }
        let _ = finish_project_review_run(
            store,
            snapshot_source,
            FinishReviewRun {
                run_id: run.id,
                project_id,
                reviewer_agent_id: run.reviewer_agent_id,
                turn_id: run.turn_id,
                status: ProjectReviewRunStatus::Cancelled,
                outcome: None,
                pr: run.pr,
                summary_text: run.summary,
                error: Some("review cancelled".to_string()),
            },
        )
        .await;
    }
    Ok(())
}

pub(crate) async fn save_project_review_run_status(
    store: &ConfigStore,
    summary: ProjectReviewRunSummary,
    messages: Vec<AgentMessage>,
    events: Vec<ServiceEvent>,
) -> Result<()> {
    store
        .save_project_review_run(&ProjectReviewRunDetail {
            summary,
            messages,
            events,
        })
        .await?;
    Ok(())
}

pub(crate) async fn update_project_review_run_turn(
    store: &ConfigStore,
    project_id: ProjectId,
    run_id: Uuid,
    reviewer_agent_id: AgentId,
    turn_id: TurnId,
) -> Result<()> {
    let Some(mut run) = store.load_project_review_run(project_id, run_id).await? else {
        return Err(RuntimeError::ProjectReviewRunNotFound(run_id));
    };
    run.summary.reviewer_agent_id = Some(reviewer_agent_id);
    run.summary.turn_id = Some(turn_id);
    run.summary.status = ProjectReviewRunStatus::Running;
    store.save_project_review_run(&run).await?;
    Ok(())
}

pub(crate) async fn finish_project_review_run(
    store: &ConfigStore,
    snapshot_source: &impl ReviewRunSnapshotSource,
    request: FinishReviewRun,
) -> Result<()> {
    let Some(existing) = store
        .load_project_review_run(request.project_id, request.run_id)
        .await?
    else {
        return Err(RuntimeError::ProjectReviewRunNotFound(request.run_id));
    };
    let reviewer_agent_id = request
        .reviewer_agent_id
        .or(existing.summary.reviewer_agent_id);
    let turn_id = request.turn_id.or(existing.summary.turn_id);
    let (messages, events) = if let Some(reviewer_agent_id) = reviewer_agent_id {
        snapshot_source.snapshot(reviewer_agent_id).await
    } else {
        (Vec::new(), Vec::new())
    };
    store
        .save_project_review_run(&ProjectReviewRunDetail {
            summary: ProjectReviewRunSummary {
                id: request.run_id,
                project_id: request.project_id,
                reviewer_agent_id,
                turn_id,
                started_at: existing.summary.started_at,
                finished_at: Some(now()),
                status: request.status,
                outcome: request.outcome,
                pr: request.pr,
                summary: request.summary_text,
                error: request.error,
            },
            messages,
            events,
        })
        .await?;
    Ok(())
}
