use std::collections::HashSet;

use mai_protocol::{
    AgentId, AgentResourceState, AgentRuntimeLifecycle, AgentSummary, ProjectId,
    ProjectReviewRunStatus, ProjectReviewRunSummary, ProjectReviewStatus, ProjectSummary,
};

use super::runs::FinishReviewRun;
use super::state::ReviewStateUpdate;
use super::worker::ProjectReviewWorkerOps;
use crate::Result;

const STARTUP_INTERRUPTED_ERROR: &str = "review interrupted by server restart";
const SELF_REPAIR_INTERRUPTED_ERROR: &str = "review interrupted by project reviewer self repair";

#[derive(Clone, Copy)]
pub(crate) enum ProjectReviewRepairReason {
    Startup,
    Runtime,
}

impl ProjectReviewRepairReason {
    fn label(self) -> &'static str {
        match self {
            ProjectReviewRepairReason::Startup => "startup",
            ProjectReviewRepairReason::Runtime => "runtime",
        }
    }

    fn interrupted_error(self) -> &'static str {
        match self {
            ProjectReviewRepairReason::Startup => STARTUP_INTERRUPTED_ERROR,
            ProjectReviewRepairReason::Runtime => SELF_REPAIR_INTERRUPTED_ERROR,
        }
    }
}

struct ProjectReviewSingletonSnapshot {
    summary: ProjectSummary,
    reviewers: Vec<AgentSummary>,
    active_runs: Vec<ProjectReviewRunSummary>,
}

pub(crate) async fn repair_project_review_singleton<Ops: ProjectReviewWorkerOps>(
    ops: &Ops,
    project_id: ProjectId,
    run_list_limit: usize,
    reason: ProjectReviewRepairReason,
) -> Result<()> {
    let snapshot = ProjectReviewSingletonSnapshot::load(ops, project_id, run_list_limit).await?;
    let keep_reviewer_id = snapshot.keep_consistent_reviewer(reason);
    let stale_activity = snapshot.has_stale_activity(keep_reviewer_id);
    if !stale_activity {
        return Ok(());
    }

    let reviewer_count = snapshot.reviewers.len();
    let active_run_count = snapshot.active_runs.len();
    let runs_to_cancel = snapshot.runs_to_cancel(keep_reviewer_id);
    let reviewer_ids_to_delete = snapshot.reviewer_ids_to_delete(keep_reviewer_id);

    let cancelled_run_count =
        cancel_project_review_runs(ops, project_id, runs_to_cancel, reason.interrupted_error())
            .await;
    let (cancelled_turn_count, deleted_reviewer_count) =
        delete_project_reviewers(ops, project_id, reviewer_ids_to_delete).await;
    if keep_reviewer_id.is_none() {
        let status = if snapshot.summary.auto_review_enabled {
            ProjectReviewStatus::Idle
        } else {
            ProjectReviewStatus::Disabled
        };
        ops.set_project_review_state(project_id, status, ReviewStateUpdate::default())
            .await?;
    }

    tracing::info!(
        project_id = %project_id,
        reason = reason.label(),
        reviewer_count,
        unfinished_run_count = active_run_count,
        cancelled_run_count,
        cancelled_turn_count,
        deleted_reviewer_count,
        repair_action = if keep_reviewer_id.is_some() {
            "trim_project_reviewer_singleton"
        } else {
            "reset_project_reviewer_singleton"
        },
        "repaired project reviewer singleton"
    );
    Ok(())
}

impl ProjectReviewSingletonSnapshot {
    async fn load<Ops: ProjectReviewWorkerOps>(
        ops: &Ops,
        project_id: ProjectId,
        run_list_limit: usize,
    ) -> Result<Self> {
        let project = ops.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let reviewers = ops.project_auto_reviewer_agents(project_id).await;
        let runs = ops
            .load_project_review_runs(project_id, 0, run_list_limit)
            .await?;
        let active_runs = runs
            .into_iter()
            .filter(project_review_run_is_active)
            .collect();
        Ok(Self {
            summary,
            reviewers,
            active_runs,
        })
    }

    fn keep_consistent_reviewer(&self, reason: ProjectReviewRepairReason) -> Option<AgentId> {
        if matches!(reason, ProjectReviewRepairReason::Startup) {
            return None;
        }
        let current_reviewer_id = self.summary.current_reviewer_agent_id?;
        let mut current_runs = self
            .active_runs
            .iter()
            .filter(|run| run.reviewer_agent_id == Some(current_reviewer_id));
        let current_run = current_runs.next()?;
        if current_runs.next().is_some() {
            return None;
        }
        let reviewer = self
            .reviewers
            .iter()
            .find(|reviewer| reviewer.id == current_reviewer_id)?;
        if !project_reviewer_agent_can_continue(reviewer) {
            return None;
        }
        (current_run.reviewer_agent_id == Some(current_reviewer_id)).then_some(current_reviewer_id)
    }

    fn has_stale_activity(&self, keep_reviewer_id: Option<AgentId>) -> bool {
        self.summary.current_reviewer_agent_id != keep_reviewer_id
            || !self.runs_to_cancel(keep_reviewer_id).is_empty()
            || self
                .reviewers
                .iter()
                .any(|reviewer| reviewer_agent_should_be_deleted(reviewer, keep_reviewer_id))
    }

    fn runs_to_cancel(&self, keep_reviewer_id: Option<AgentId>) -> Vec<ProjectReviewRunSummary> {
        self.active_runs
            .iter()
            .filter(|run| keep_reviewer_id.is_none_or(|id| run.reviewer_agent_id != Some(id)))
            .cloned()
            .collect()
    }

    fn reviewer_ids_to_delete(&self, keep_reviewer_id: Option<AgentId>) -> Vec<AgentId> {
        let mut reviewer_ids = HashSet::new();
        if let Some(reviewer_id) = self
            .summary
            .current_reviewer_agent_id
            .filter(|reviewer_id| Some(*reviewer_id) != keep_reviewer_id)
        {
            reviewer_ids.insert(reviewer_id);
        }
        for reviewer in &self.reviewers {
            if reviewer_agent_should_be_deleted(reviewer, keep_reviewer_id) {
                reviewer_ids.insert(reviewer.id);
            }
        }
        for run in &self.active_runs {
            if keep_reviewer_id.is_some_and(|id| run.reviewer_agent_id == Some(id)) {
                continue;
            }
            if let Some(reviewer_id) = run.reviewer_agent_id {
                reviewer_ids.insert(reviewer_id);
            }
        }
        if let Some(keep_reviewer_id) = keep_reviewer_id {
            reviewer_ids.remove(&keep_reviewer_id);
        }
        let mut reviewer_ids = reviewer_ids.into_iter().collect::<Vec<_>>();
        reviewer_ids.sort();
        reviewer_ids
    }
}

async fn cancel_project_review_runs<Ops: ProjectReviewWorkerOps>(
    ops: &Ops,
    project_id: ProjectId,
    runs: Vec<ProjectReviewRunSummary>,
    error: &str,
) -> usize {
    let mut cancelled_run_count = 0;
    for run in runs {
        match ops
            .finish_project_review_run(FinishReviewRun {
                run_id: run.id,
                project_id,
                reviewer_agent_id: run.reviewer_agent_id,
                turn_id: run.turn_id,
                status: ProjectReviewRunStatus::Cancelled,
                outcome: None,
                review_event: None,
                pr: run.pr,
                summary_text: run.summary,
                error: Some(error.to_string()),
            })
            .await
        {
            Ok(()) => {
                cancelled_run_count += 1;
            }
            Err(err) => {
                tracing::warn!(
                    project_id = %project_id,
                    run_id = %run.id,
                    "failed to cancel stale project review run during singleton repair: {err}"
                );
            }
        }
    }
    cancelled_run_count
}

async fn delete_project_reviewers<Ops: ProjectReviewWorkerOps>(
    ops: &Ops,
    project_id: ProjectId,
    reviewer_ids: Vec<AgentId>,
) -> (usize, usize) {
    let mut cancelled_turn_count = 0;
    let mut deleted_reviewer_count = 0;
    for reviewer_id in reviewer_ids {
        match ops.agent_current_turn(reviewer_id).await {
            Ok(Some(turn_id)) => match ops.cancel_agent_turn(reviewer_id, turn_id).await {
                Ok(()) => {
                    cancelled_turn_count += 1;
                }
                Err(err) => {
                    tracing::warn!(
                        project_id = %project_id,
                        reviewer_id = %reviewer_id,
                        turn_id = %turn_id,
                        "failed to cancel stale project reviewer turn during singleton repair: {err}"
                    );
                }
            },
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    project_id = %project_id,
                    reviewer_id = %reviewer_id,
                    "failed to read stale project reviewer turn during singleton repair: {err}"
                );
            }
        }
        match ops.delete_agent(reviewer_id).await {
            Ok(()) => {
                deleted_reviewer_count += 1;
            }
            Err(err) => {
                tracing::warn!(
                    project_id = %project_id,
                    reviewer_id = %reviewer_id,
                    "failed to delete stale project reviewer agent during singleton repair: {err}"
                );
            }
        }
    }
    (cancelled_turn_count, deleted_reviewer_count)
}

fn project_review_run_is_active(run: &ProjectReviewRunSummary) -> bool {
    run.finished_at.is_none()
        && matches!(
            run.status,
            ProjectReviewRunStatus::Syncing | ProjectReviewRunStatus::Running
        )
}

fn reviewer_agent_should_be_deleted(
    reviewer: &AgentSummary,
    keep_reviewer_id: Option<AgentId>,
) -> bool {
    Some(reviewer.id) != keep_reviewer_id
}

fn project_reviewer_agent_can_continue(reviewer: &AgentSummary) -> bool {
    matches!(
        reviewer.state.resource,
        AgentResourceState::Provisioning | AgentResourceState::Ready
    ) && reviewer.state.runtime.lifecycle == AgentRuntimeLifecycle::Active
}
