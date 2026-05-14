use std::future::Future;
use std::sync::Arc;

use mai_protocol::{
    AgentDetail, AgentId, AgentModelPreference, AgentSummary, PlanStatus, ServiceEventKind,
    TaskDetail, TaskId, TaskPlan, TaskStatus, TaskSummary, TurnId, now,
};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

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

pub(crate) struct CreateTaskInput {
    pub(crate) title: Option<String>,
    pub(crate) initial_message: Option<String>,
    pub(crate) docker_image: Option<String>,
}

pub(crate) struct CreateTaskPlannerAgentRequest {
    pub(crate) task_id: TaskId,
    pub(crate) title: String,
    pub(crate) model: AgentModelPreference,
    pub(crate) docker_image: Option<String>,
}

/// Supplies agent/model side effects needed by task creation.
pub(crate) trait TaskCreateOps: Send + Sync {
    fn planner_model(&self) -> impl Future<Output = Result<AgentModelPreference>> + Send;

    fn create_task_planner_agent(
        &self,
        request: CreateTaskPlannerAgentRequest,
    ) -> impl Future<Output = Result<AgentSummary>> + Send;

    fn save_task(
        &self,
        summary: &TaskSummary,
        plan: &TaskPlan,
    ) -> impl Future<Output = Result<()>> + Send;

    fn publish_task_event(&self, event: ServiceEventKind) -> impl Future<Output = ()> + Send;

    fn send_task_message(
        &self,
        task_id: TaskId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> impl Future<Output = Result<TurnId>> + Send;

    fn spawn_task_title_generation(
        &self,
        task_id: TaskId,
        message: String,
    ) -> impl Future<Output = ()> + Send;
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

pub(crate) async fn create_task(
    state: &RuntimeState,
    ops: &impl TaskCreateOps,
    input: CreateTaskInput,
) -> Result<TaskSummary> {
    let task_id = Uuid::new_v4();
    let user_omitted_title = input
        .title
        .as_ref()
        .map(|value| value.trim().is_empty())
        .unwrap_or(true);
    let title = input
        .title
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "New Task".to_string());
    let planner_model = ops.planner_model().await?;
    let created_at = now();
    let planner = ops
        .create_task_planner_agent(CreateTaskPlannerAgentRequest {
            task_id,
            title: title.clone(),
            model: planner_model,
            docker_image: input.docker_image,
        })
        .await?;
    let plan = TaskPlan::default();
    let summary = TaskSummary {
        id: task_id,
        title,
        status: TaskStatus::Planning,
        plan_status: plan.status.clone(),
        plan_version: plan.version,
        planner_agent_id: planner.id,
        current_agent_id: Some(planner.id),
        agent_count: 1,
        review_rounds: 0,
        created_at,
        updated_at: now(),
        last_error: None,
        final_report: None,
    };
    ops.save_task(&summary, &plan).await?;
    state.tasks.write().await.insert(
        task_id,
        Arc::new(TaskRecord {
            summary: RwLock::new(summary.clone()),
            plan: RwLock::new(plan),
            plan_history: RwLock::new(Vec::new()),
            reviews: RwLock::new(Vec::new()),
            artifacts: RwLock::new(Vec::new()),
            workflow_lock: Mutex::new(()),
        }),
    );
    ops.publish_task_event(ServiceEventKind::TaskCreated {
        task: summary.clone(),
    })
    .await;
    let message_for_title = input
        .initial_message
        .as_ref()
        .filter(|message| !message.trim().is_empty())
        .cloned();
    if let Some(message) = input
        .initial_message
        .filter(|message| !message.trim().is_empty())
    {
        let _ = ops.send_task_message(task_id, message, Vec::new()).await?;
    }
    if user_omitted_title && let Some(message_text) = message_for_title {
        ops.spawn_task_title_generation(task_id, message_text).await;
    }
    Ok(summary)
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
