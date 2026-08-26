use std::collections::HashSet;

use mai_protocol::{
    AgentId, AgentResourceState, AgentSummary, ProjectId, ProjectReviewJobSummary,
    ProjectReviewRunStatus, ProjectReviewRunSummary, ProjectReviewStatus, ProjectSummary,
};

use super::runs::FinishReviewRun;
use super::state::{ReviewStateUpdate, ReviewerAgentUpdate};
use super::worker::ProjectReviewWorkerOps;
use crate::Result;

const STARTUP_INTERRUPTED_ERROR: &str = "review interrupted by server restart";
#[cfg(test)]
const SELF_REPAIR_INTERRUPTED_ERROR: &str = "review interrupted by project reviewer self repair";

#[derive(Clone, Copy)]
pub(crate) enum ProjectReviewRepairReason {
    Startup,
    #[cfg(test)]
    Runtime,
}

impl ProjectReviewRepairReason {
    fn label(self) -> &'static str {
        match self {
            ProjectReviewRepairReason::Startup => "startup",
            #[cfg(test)]
            ProjectReviewRepairReason::Runtime => "runtime",
        }
    }

    fn interrupted_error(self) -> &'static str {
        match self {
            ProjectReviewRepairReason::Startup => STARTUP_INTERRUPTED_ERROR,
            #[cfg(test)]
            ProjectReviewRepairReason::Runtime => SELF_REPAIR_INTERRUPTED_ERROR,
        }
    }

    fn preserves_active_run(self) -> bool {
        match self {
            ProjectReviewRepairReason::Startup => false,
            #[cfg(test)]
            ProjectReviewRepairReason::Runtime => true,
        }
    }
}

struct ProjectReviewSingletonSnapshot {
    summary: ProjectSummary,
    reviewers: Vec<AgentSummary>,
    active_runs: Vec<ProjectReviewRunSummary>,
    active_job: Option<ProjectReviewJobSummary>,
}

pub(crate) async fn repair_project_review_singleton<Ops: ProjectReviewWorkerOps>(
    ops: &Ops,
    project_id: ProjectId,
    run_list_limit: usize,
    reason: ProjectReviewRepairReason,
) -> Result<()> {
    let snapshot = ProjectReviewSingletonSnapshot::load(ops, project_id, run_list_limit).await?;
    let keep_reviewer_id = snapshot.keep_consistent_reviewer();
    let stale_activity = snapshot.has_stale_activity(keep_reviewer_id, reason);
    if !stale_activity {
        return Ok(());
    }

    let reviewer_count = snapshot.reviewers.len();
    let active_run_count = snapshot.active_runs.len();
    let runs_to_cancel = snapshot.runs_to_cancel(keep_reviewer_id, reason);
    let reviewer_ids_to_delete = snapshot.reviewer_ids_to_delete(keep_reviewer_id);

    let cancelled_run_count =
        cancel_project_review_runs(ops, project_id, runs_to_cancel, reason.interrupted_error())
            .await?;
    let preserved_reviewer_cancelled_turn_count = match keep_reviewer_id {
        Some(reviewer_id) if !reason.preserves_active_run() => {
            cancel_project_reviewer_turn(ops, project_id, reviewer_id).await
        }
        Some(_) | None => 0,
    };
    let (deleted_reviewer_cancelled_turn_count, deleted_reviewer_count) =
        delete_project_reviewers(ops, project_id, reviewer_ids_to_delete).await?;
    let cancelled_turn_count =
        preserved_reviewer_cancelled_turn_count + deleted_reviewer_cancelled_turn_count;
    match keep_reviewer_id {
        Some(reviewer_id) if snapshot.summary.current_reviewer_agent_id != Some(reviewer_id) => {
            let active_job = snapshot
                .active_job
                .as_ref()
                .expect("kept reviewer must belong to an active Job");
            let reviewer_update = if snapshot.summary.current_reviewer_agent_id == Some(reviewer_id)
            {
                ReviewerAgentUpdate::Keep
            } else {
                ReviewerAgentUpdate::Set(reviewer_id)
            };
            ops.set_project_review_state(
                project_id,
                super::job::project_review_status_for_job(
                    snapshot.summary.auto_review_enabled,
                    Some(active_job),
                ),
                ReviewStateUpdate {
                    current_reviewer_agent_id: reviewer_update,
                    next_review_at: active_job.next_attempt_at,
                    error: active_job
                        .failure
                        .as_ref()
                        .map(|failure| failure.message.clone()),
                    ..Default::default()
                },
            )
            .await?;
        }
        Some(_) => {}
        None => {
            let status = if snapshot.summary.auto_review_enabled {
                ProjectReviewStatus::Idle
            } else {
                ProjectReviewStatus::Disabled
            };
            ops.set_project_review_state(project_id, status, ReviewStateUpdate::default())
                .await?;
        }
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
        let active_job = ops.load_active_project_review_job(project_id).await?;
        let active_runs = runs
            .into_iter()
            .filter(project_review_run_is_active)
            .collect();
        Ok(Self {
            summary,
            reviewers,
            active_runs,
            active_job,
        })
    }

    fn keep_consistent_reviewer(&self) -> Option<AgentId> {
        let active_job = self.active_job.as_ref()?;
        let current_reviewer_id = active_job.reviewer_agent_id?;
        let reviewer = self
            .reviewers
            .iter()
            .find(|reviewer| reviewer.id == current_reviewer_id)?;
        project_reviewer_agent_can_continue(reviewer).then_some(current_reviewer_id)
    }

    fn has_stale_activity(
        &self,
        keep_reviewer_id: Option<AgentId>,
        reason: ProjectReviewRepairReason,
    ) -> bool {
        self.summary.current_reviewer_agent_id != keep_reviewer_id
            || !self.runs_to_cancel(keep_reviewer_id, reason).is_empty()
            || self
                .reviewers
                .iter()
                .any(|reviewer| reviewer_agent_should_be_deleted(reviewer, keep_reviewer_id))
    }

    fn runs_to_cancel(
        &self,
        keep_reviewer_id: Option<AgentId>,
        reason: ProjectReviewRepairReason,
    ) -> Vec<ProjectReviewRunSummary> {
        self.active_runs
            .iter()
            .filter(|run| {
                !reason.preserves_active_run()
                    || keep_reviewer_id.is_none_or(|id| run.reviewer_agent_id != Some(id))
            })
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
) -> Result<usize> {
    let mut cancelled_run_count = 0;
    for run in runs {
        ops.finish_project_review_run(FinishReviewRun {
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
            failure: None,
        })
        .await?;
        cancelled_run_count += 1;
    }
    Ok(cancelled_run_count)
}

async fn delete_project_reviewers<Ops: ProjectReviewWorkerOps>(
    ops: &Ops,
    project_id: ProjectId,
    reviewer_ids: Vec<AgentId>,
) -> Result<(usize, usize)> {
    let mut cancelled_turn_count = 0;
    let mut deleted_reviewer_count = 0;
    for reviewer_id in reviewer_ids {
        cancelled_turn_count += cancel_project_reviewer_turn(ops, project_id, reviewer_id).await;
        match ops.delete_agent(reviewer_id).await {
            Ok(()) => {
                deleted_reviewer_count += 1;
            }
            Err(crate::RuntimeError::AgentNotFound(missing_id)) if missing_id == reviewer_id => {
                deleted_reviewer_count += 1;
            }
            Err(err) => {
                return Err(err);
            }
        }
    }
    Ok((cancelled_turn_count, deleted_reviewer_count))
}

async fn cancel_project_reviewer_turn<Ops: ProjectReviewWorkerOps>(
    ops: &Ops,
    project_id: ProjectId,
    reviewer_id: AgentId,
) -> usize {
    match ops.agent_current_turn(reviewer_id).await {
        Ok(Some(turn_id)) => match ops.cancel_agent_turn(reviewer_id, turn_id.clone()).await {
            Ok(()) => 1,
            Err(err) => {
                tracing::warn!(
                    project_id = %project_id,
                    reviewer_id = %reviewer_id,
                    turn_id = %turn_id,
                    "failed to cancel stale project reviewer turn during singleton repair: {err}"
                );
                0
            }
        },
        Ok(None) => 0,
        Err(err) => {
            tracing::warn!(
                project_id = %project_id,
                reviewer_id = %reviewer_id,
                "failed to read stale project reviewer turn during singleton repair: {err}"
            );
            0
        }
    }
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
        reviewer.resource.state,
        AgentResourceState::Provisioning | AgentResourceState::Ready
    ) && reviewer
        .runtime
        .as_ref()
        .is_some_and(|snapshot| snapshot.state.is_operational())
}
