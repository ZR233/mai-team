use std::future::Future;
use std::sync::Arc;

use mai_protocol::{
    AgentDetail, AgentId, AgentSummary, PlanStatus, TaskDetail, TaskId, TaskPlan, TaskStatus,
    TaskSummary, now,
};

use crate::state::{RuntimeState, TaskRecord};
use crate::{Result, RuntimeError};

/// Supplies agent read models needed to assemble task detail responses.
pub(crate) trait TaskReadOps: Send + Sync {
    fn get_agent(
        &self,
        agent_id: AgentId,
        session_id: Option<mai_protocol::SessionId>,
    ) -> impl Future<Output = Result<AgentDetail>> + Send;
}

/// Supplies persistence and event side effects for task summary/plan updates.
pub(crate) trait TaskUpdateOps: Send + Sync {
    fn save_task(
        &self,
        summary: &TaskSummary,
        plan: &TaskPlan,
    ) -> impl Future<Output = Result<()>> + Send;

    fn publish_task_updated(&self, task: TaskSummary) -> impl Future<Output = ()> + Send;
}

pub(crate) async fn task(state: &RuntimeState, task_id: TaskId) -> Result<Arc<TaskRecord>> {
    state
        .tasks
        .read()
        .await
        .get(&task_id)
        .cloned()
        .ok_or(RuntimeError::TaskNotFound(task_id))
}

pub(crate) async fn list_tasks(state: &RuntimeState) -> Vec<TaskSummary> {
    let task_records = {
        let tasks = state.tasks.read().await;
        tasks.values().cloned().collect::<Vec<_>>()
    };
    let mut summaries = Vec::with_capacity(task_records.len());
    for task in task_records {
        let mut summary = task.summary.read().await.clone();
        refresh_summary_counts(state, &mut summary).await;
        summaries.push(summary);
    }
    summaries.sort_by_key(|summary| summary.created_at);
    summaries
}

pub(crate) async fn get_task(
    state: &RuntimeState,
    ops: &impl TaskReadOps,
    task_id: TaskId,
    selected_agent_id: Option<AgentId>,
) -> Result<TaskDetail> {
    let task = task(state, task_id).await?;
    let summary = task_summary(state, &task).await;
    let plan = task.plan.read().await.clone();
    let plan_history = task.plan_history.read().await.clone();
    let reviews = task.reviews.read().await.clone();
    let agents = task_agents(state, task_id).await;
    let selected_agent_id = selected_agent_id
        .filter(|id| agents.iter().any(|agent| agent.id == *id))
        .or(summary.current_agent_id)
        .unwrap_or(summary.planner_agent_id);
    let selected_agent = ops.get_agent(selected_agent_id, None).await?;
    Ok(TaskDetail {
        summary,
        plan,
        plan_history,
        reviews,
        agents,
        selected_agent_id,
        selected_agent,
        artifacts: task.artifacts.read().await.clone(),
    })
}

pub(crate) async fn task_summary(state: &RuntimeState, task: &Arc<TaskRecord>) -> TaskSummary {
    let mut summary = task.summary.read().await.clone();
    refresh_summary_counts(state, &mut summary).await;
    summary
}

pub(crate) async fn refresh_summary_counts(state: &RuntimeState, summary: &mut TaskSummary) {
    summary.agent_count = task_agents(state, summary.id).await.len();
    let task = {
        let tasks = state.tasks.read().await;
        tasks.get(&summary.id).cloned()
    };
    if let Some(task) = task {
        summary.review_rounds = task.reviews.read().await.len() as u64;
    }
}

pub(crate) async fn task_agents(state: &RuntimeState, task_id: TaskId) -> Vec<AgentSummary> {
    let agents = state.agents.read().await;
    let mut summaries = Vec::new();
    for agent in agents.values() {
        let summary = agent.summary.read().await.clone();
        if summary.task_id == Some(task_id) {
            summaries.push(summary);
        }
    }
    summaries.sort_by_key(|summary| summary.created_at);
    summaries
}

pub(crate) async fn update_task_title(
    state: &RuntimeState,
    ops: &impl TaskUpdateOps,
    task_id: TaskId,
    new_title: String,
) -> Result<()> {
    let task = task(state, task_id).await?;
    let plan = task.plan.read().await.clone();
    let mut updated = {
        let mut summary = task.summary.write().await;
        summary.title = new_title;
        summary.updated_at = now();
        refresh_summary_counts(state, &mut summary).await;
        summary.clone()
    };
    sync_plan_fields(&mut updated, &plan);
    ops.save_task(&updated, &plan).await?;
    ops.publish_task_updated(updated).await;
    Ok(())
}

pub(crate) async fn set_current_agent(
    state: &RuntimeState,
    ops: &impl TaskUpdateOps,
    task: &Arc<TaskRecord>,
    agent_id: AgentId,
    status: TaskStatus,
    error: Option<String>,
) -> Result<()> {
    let plan = task.plan.read().await.clone();
    let updated = {
        let mut summary = task.summary.write().await;
        summary.current_agent_id = Some(agent_id);
        summary.status = status;
        summary.updated_at = now();
        if let Some(error) = error {
            summary.last_error = Some(error);
        }
        sync_plan_fields(&mut summary, &plan);
        refresh_summary_counts(state, &mut summary).await;
        summary.clone()
    };
    ops.save_task(&updated, &plan).await?;
    ops.publish_task_updated(updated).await;
    Ok(())
}

pub(crate) async fn set_status(
    state: &RuntimeState,
    ops: &impl TaskUpdateOps,
    task: &Arc<TaskRecord>,
    status: TaskStatus,
    final_report: Option<String>,
    error: Option<String>,
) -> Result<()> {
    let plan = task.plan.read().await.clone();
    let updated = {
        let mut summary = task.summary.write().await;
        summary.status = status;
        summary.updated_at = now();
        if final_report.is_some() {
            summary.final_report = final_report;
        }
        if error.is_some() {
            summary.last_error = error;
        }
        sync_plan_fields(&mut summary, &plan);
        refresh_summary_counts(state, &mut summary).await;
        summary.clone()
    };
    ops.save_task(&updated, &plan).await?;
    ops.publish_task_updated(updated).await;
    Ok(())
}

fn sync_plan_fields(summary: &mut TaskSummary, plan: &TaskPlan) {
    summary.plan_status = plan.status.clone();
    summary.plan_version = plan.version;
    if plan.status == PlanStatus::Missing {
        summary.plan_version = 0;
    }
}
