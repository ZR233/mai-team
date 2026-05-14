use std::future::Future;
use std::time::Duration;

use mai_protocol::{AgentId, AgentRole, AgentStatus, AgentSummary, TaskId, TaskStatus, TurnId};

use crate::state::RuntimeState;
use crate::{Result, RuntimeError};

use super::{TaskUpdateOps, set_current_agent, set_status, task};

const REVIEW_ROUND_LIMIT: u64 = 5;
const TASK_AGENT_WAIT_TIMEOUT: Duration = Duration::from_secs(3600);

/// Supplies agent orchestration side effects for task execution workflows.
pub(crate) trait TaskWorkflowOps: TaskUpdateOps {
    fn spawn_task_role_agent(
        &self,
        parent_agent_id: AgentId,
        role: AgentRole,
        name: Option<String>,
    ) -> impl Future<Output = Result<AgentSummary>> + Send;

    fn start_agent_turn(
        &self,
        agent_id: AgentId,
        message: String,
    ) -> impl Future<Output = Result<TurnId>> + Send;

    fn wait_agent(
        &self,
        agent_id: AgentId,
        timeout: Duration,
    ) -> impl Future<Output = Result<AgentSummary>> + Send;
}

pub(crate) async fn run_task_workflow(
    state: &RuntimeState,
    ops: &impl TaskWorkflowOps,
    task_id: TaskId,
) -> Result<()> {
    let task = task(state, task_id).await?;
    let _workflow_guard = task.workflow_lock.lock().await;
    let plan_markdown = task
        .plan
        .read()
        .await
        .markdown
        .clone()
        .filter(|plan| !plan.trim().is_empty())
        .ok_or_else(|| RuntimeError::InvalidInput("approved plan is empty".to_string()))?;
    let planner_agent_id = task.summary.read().await.planner_agent_id;
    let executor = ops
        .spawn_task_role_agent(
            planner_agent_id,
            AgentRole::Executor,
            Some("Task Executor".to_string()),
        )
        .await?;
    set_current_agent(state, ops, &task, executor.id, TaskStatus::Executing, None).await?;
    ops.start_agent_turn(
        executor.id,
        format!(
            "Implement the approved task plan below. Keep changes scoped, run verification, and report touched files and test results.\n\n{}",
            plan_markdown
        ),
    )
    .await?;
    let mut executor_summary = ops.wait_agent(executor.id, TASK_AGENT_WAIT_TIMEOUT).await?;
    for round in 1..=REVIEW_ROUND_LIMIT {
        if matches!(
            executor_summary.status,
            AgentStatus::Failed | AgentStatus::Cancelled
        ) {
            return Err(RuntimeError::InvalidInput(format!(
                "executor ended with status {:?}",
                executor_summary.status
            )));
        }
        let reviewer = ops
            .spawn_task_role_agent(
                executor.id,
                AgentRole::Reviewer,
                Some(format!("Task Reviewer {round}")),
            )
            .await?;
        set_current_agent(state, ops, &task, reviewer.id, TaskStatus::Reviewing, None).await?;
        ops.start_agent_turn(
            reviewer.id,
            format!(
                "Review the executor's changes for the approved task plan. Use submit_review_result with passed=true only when there are no blocking issues. Include concrete findings and a concise summary.\n\nApproved plan:\n{}",
                plan_markdown
            ),
        )
        .await?;
        let reviewer_summary = ops.wait_agent(reviewer.id, TASK_AGENT_WAIT_TIMEOUT).await?;
        if matches!(
            reviewer_summary.status,
            AgentStatus::Failed | AgentStatus::Cancelled
        ) {
            return Err(RuntimeError::InvalidInput(format!(
                "reviewer ended with status {:?}",
                reviewer_summary.status
            )));
        }
        let latest_review = task.reviews.read().await.last().cloned();
        let Some(review) = latest_review else {
            return Err(RuntimeError::InvalidInput(
                "reviewer did not submit a review result".to_string(),
            ));
        };
        if review.passed {
            let report = if review.summary.trim().is_empty() {
                "Task completed and review passed.".to_string()
            } else {
                review.summary.clone()
            };
            set_status(state, ops, &task, TaskStatus::Completed, Some(report), None).await?;
            return Ok(());
        }
        if round == REVIEW_ROUND_LIMIT {
            set_status(
                state,
                ops,
                &task,
                TaskStatus::Failed,
                None,
                Some(format!(
                    "review did not pass after {REVIEW_ROUND_LIMIT} rounds: {}",
                    review.findings
                )),
            )
            .await?;
            return Ok(());
        }
        set_current_agent(state, ops, &task, executor.id, TaskStatus::Executing, None).await?;
        ops.start_agent_turn(
            executor.id,
            format!(
                "The reviewer found issues. Fix them, rerun verification, and report the changes.\n\nReview findings:\n{}\n\nReview summary:\n{}",
                review.findings, review.summary
            ),
        )
        .await?;
        executor_summary = ops.wait_agent(executor.id, TASK_AGENT_WAIT_TIMEOUT).await?;
    }
    Ok(())
}
