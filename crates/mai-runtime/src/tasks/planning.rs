use mai_protocol::{PlanHistoryEntry, PlanStatus, TaskId, TaskStatus, TaskSummary, TurnId, now};

use crate::state::RuntimeState;
use crate::{Result, RuntimeError};

use super::{TaskPlanningOps, refresh_summary_counts, set_current_agent, task, task_summary};

pub(crate) async fn send_task_message(
    state: &RuntimeState,
    ops: &impl TaskPlanningOps,
    task_id: TaskId,
    message: String,
    skill_mentions: Vec<String>,
) -> Result<TurnId> {
    let task = task(state, task_id).await?;
    let planner_agent_id = task.summary.read().await.planner_agent_id;
    {
        let mut plan = task.plan.write().await;
        if matches!(plan.status, PlanStatus::Ready | PlanStatus::Approved) {
            let entry = PlanHistoryEntry {
                version: plan.version,
                title: plan.title.clone(),
                markdown: plan.markdown.clone(),
                saved_at: plan.saved_at,
                saved_by_agent_id: plan.saved_by_agent_id,
                revision_feedback: None,
                revision_requested_at: None,
            };
            ops.save_plan_history_entry(task_id, &entry).await?;
            task.plan_history.write().await.push(entry);
            plan.status = PlanStatus::NeedsRevision;
            plan.revision_feedback = None;
            plan.revision_requested_at = None;
            plan.approved_at = None;
            let updated = {
                let mut summary = task.summary.write().await;
                summary.status = TaskStatus::Planning;
                summary.plan_status = PlanStatus::NeedsRevision;
                summary.final_report = None;
                summary.last_error = None;
                summary.updated_at = now();
                refresh_summary_counts(state, &mut summary).await;
                summary.clone()
            };
            ops.save_task(&updated, &plan).await?;
            ops.publish_plan_updated(task_id, plan.clone()).await;
            ops.publish_task_updated(updated).await;
        }
    }
    let turn_id = ops
        .send_agent_message(planner_agent_id, message, skill_mentions)
        .await?;
    set_current_agent(
        state,
        ops,
        &task,
        planner_agent_id,
        TaskStatus::Planning,
        None,
    )
    .await?;
    Ok(turn_id)
}

pub(crate) async fn approve_task_plan(
    state: &RuntimeState,
    ops: &impl TaskPlanningOps,
    task_id: TaskId,
) -> Result<TaskSummary> {
    let task = task(state, task_id).await?;
    {
        let mut plan = task.plan.write().await;
        if plan.status != PlanStatus::Ready || plan.markdown.as_deref().unwrap_or("").is_empty() {
            return Err(RuntimeError::InvalidInput(
                "task has no ready plan to approve".to_string(),
            ));
        }
        plan.status = PlanStatus::Approved;
        plan.approved_at = Some(now());
        let updated = {
            let mut summary = task.summary.write().await;
            summary.status = TaskStatus::Executing;
            summary.plan_status = PlanStatus::Approved;
            summary.plan_version = plan.version;
            summary.updated_at = now();
            refresh_summary_counts(state, &mut summary).await;
            summary.clone()
        };
        ops.save_task(&updated, &plan).await?;
        ops.publish_task_updated(updated).await;
    }
    ops.spawn_task_workflow(task_id).await;
    Ok(task_summary(state, &task).await)
}

pub(crate) async fn request_plan_revision(
    state: &RuntimeState,
    ops: &impl TaskPlanningOps,
    task_id: TaskId,
    feedback: String,
) -> Result<TaskSummary> {
    let task = task(state, task_id).await?;
    {
        let mut plan = task.plan.write().await;
        if plan.status != PlanStatus::Ready {
            return Err(RuntimeError::InvalidInput(
                "task plan is not in ready status".to_string(),
            ));
        }
        let entry = PlanHistoryEntry {
            version: plan.version,
            title: plan.title.clone(),
            markdown: plan.markdown.clone(),
            saved_at: plan.saved_at,
            saved_by_agent_id: plan.saved_by_agent_id,
            revision_feedback: Some(feedback.clone()),
            revision_requested_at: Some(now()),
        };
        ops.save_plan_history_entry(task_id, &entry).await?;
        task.plan_history.write().await.push(entry);
        plan.status = PlanStatus::NeedsRevision;
        plan.revision_feedback = Some(feedback.clone());
        plan.revision_requested_at = Some(now());
        let updated = {
            let mut summary = task.summary.write().await;
            summary.status = TaskStatus::Planning;
            summary.plan_status = PlanStatus::NeedsRevision;
            summary.updated_at = now();
            refresh_summary_counts(state, &mut summary).await;
            summary.clone()
        };
        ops.save_task(&updated, &plan).await?;
        ops.publish_plan_updated(task_id, plan.clone()).await;
        ops.publish_task_updated(updated).await;
    }
    let planner_agent_id = task.summary.read().await.planner_agent_id;
    let feedback_message = format!(
        "The user requests revision of the plan.\n\nFeedback:\n{feedback}\n\nPlease address the feedback and save an updated plan."
    );
    let _ = ops
        .send_agent_message(planner_agent_id, feedback_message, Vec::new())
        .await?;
    set_current_agent(
        state,
        ops,
        &task,
        planner_agent_id,
        TaskStatus::Planning,
        None,
    )
    .await?;
    Ok(task_summary(state, &task).await)
}
