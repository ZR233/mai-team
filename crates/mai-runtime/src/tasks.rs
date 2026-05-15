use std::future::Future;
use std::sync::Arc;

use mai_protocol::{
    AgentDetail, AgentId, AgentModelPreference, AgentRole, AgentSessionSummary, AgentSummary,
    EnvironmentDetail, EnvironmentId, EnvironmentSummary, PlanHistoryEntry, PlanStatus,
    ServiceEventKind, SessionId, TaskDetail, TaskId, TaskPlan, TaskReview, TaskStatus, TaskSummary,
    TurnId, now,
};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::state::{RuntimeState, TaskRecord};
use crate::{Result, RuntimeError};

mod artifacts;
mod lifecycle;
mod planning;
mod workflow;

pub(crate) use artifacts::{TaskArtifactOps, artifact_file_path, save_artifact};
pub(crate) use lifecycle::{TaskLifecycleOps, cancel_task, delete_task};
pub(crate) use planning::{
    TaskPlanningOps, approve_task_plan, request_plan_revision, send_task_message,
};
pub(crate) use workflow::{TaskWorkflowOps, run_task_workflow};

pub(crate) const DEFAULT_ENVIRONMENT_NAME: &str = "默认环境";

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

/// Supplies persistence and event side effects for task plan mutations.
pub(crate) trait TaskPlanOps: TaskUpdateOps {
    fn save_plan_history_entry(
        &self,
        task_id: TaskId,
        entry: &PlanHistoryEntry,
    ) -> impl Future<Output = Result<()>> + Send;
    fn publish_plan_updated(
        &self,
        task_id: TaskId,
        plan: TaskPlan,
    ) -> impl Future<Output = ()> + Send;
}

/// Supplies agent and persistence side effects for task tool mutations.
pub(crate) trait TaskToolOps: TaskPlanOps {
    fn agent_summary(&self, agent_id: AgentId)
    -> impl Future<Output = Result<AgentSummary>> + Send;
    fn append_task_review(&self, review: &TaskReview) -> impl Future<Output = Result<()>> + Send;
}

impl<T> TaskUpdateOps for Arc<T>
where
    T: TaskUpdateOps + Send + Sync,
{
    async fn save_task(&self, summary: &TaskSummary, plan: &TaskPlan) -> Result<()> {
        self.as_ref().save_task(summary, plan).await
    }

    async fn publish_task_updated(&self, task: TaskSummary) {
        self.as_ref().publish_task_updated(task).await;
    }
}

impl<T> TaskPlanOps for Arc<T>
where
    T: TaskPlanOps + Send + Sync,
{
    async fn save_plan_history_entry(
        &self,
        task_id: TaskId,
        entry: &PlanHistoryEntry,
    ) -> Result<()> {
        self.as_ref().save_plan_history_entry(task_id, entry).await
    }

    async fn publish_plan_updated(&self, task_id: TaskId, plan: TaskPlan) {
        self.as_ref().publish_plan_updated(task_id, plan).await;
    }
}

pub(crate) struct CreateTaskInput {
    pub(crate) title: Option<String>,
    pub(crate) initial_message: Option<String>,
    pub(crate) docker_image: Option<String>,
}

pub(crate) struct CreateEnvironmentInput {
    pub(crate) name: String,
    pub(crate) docker_image: Option<String>,
}

pub(crate) struct CreateTaskPlannerAgentRequest {
    pub(crate) task_id: TaskId,
    pub(crate) title: String,
    pub(crate) model: AgentModelPreference,
    pub(crate) docker_image: Option<String>,
}

pub(crate) struct CreateEnvironmentRootAgentRequest {
    pub(crate) environment_id: EnvironmentId,
    pub(crate) name: String,
    pub(crate) model: Option<AgentModelPreference>,
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

/// Supplies root agent creation and turn side effects for chat environments.
pub(crate) trait EnvironmentOps: TaskUpdateOps {
    fn environment_model(&self) -> impl Future<Output = Result<Option<AgentModelPreference>>> + Send;

    fn create_environment_root_agent(
        &self,
        request: CreateEnvironmentRootAgentRequest,
    ) -> impl Future<Output = Result<AgentSummary>> + Send;

    fn get_agent(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> impl Future<Output = Result<AgentDetail>> + Send;

    fn create_agent_session(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<AgentSessionSummary>> + Send;

    fn send_agent_message(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> impl Future<Output = Result<TurnId>> + Send;
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

pub(crate) async fn list_environments(state: &RuntimeState) -> Vec<EnvironmentSummary> {
    let task_records = {
        let tasks = state.tasks.read().await;
        tasks.values().cloned().collect::<Vec<_>>()
    };
    let mut summaries = Vec::with_capacity(task_records.len());
    for task in task_records {
        let task_summary = task.summary.read().await.clone();
        if let Some(summary) = environment_summary(state, &task_summary).await {
            summaries.push(summary);
        }
    }
    summaries.sort_by_key(|summary| summary.created_at);
    summaries
}

pub(crate) async fn create_environment(
    state: &RuntimeState,
    ops: &impl EnvironmentOps,
    input: CreateEnvironmentInput,
) -> Result<EnvironmentSummary> {
    let environment_id = Uuid::new_v4();
    let name = input.name.trim();
    let name = if name.is_empty() {
        DEFAULT_ENVIRONMENT_NAME.to_string()
    } else {
        name.to_string()
    };
    let model = ops.environment_model().await?;
    let created_at = now();
    let root_agent = ops
        .create_environment_root_agent(CreateEnvironmentRootAgentRequest {
            environment_id,
            name: name.clone(),
            model,
            docker_image: input.docker_image,
        })
        .await?;
    let plan = TaskPlan::default();
    let task = TaskSummary {
        id: environment_id,
        title: name,
        status: TaskStatus::Planning,
        plan_status: plan.status.clone(),
        plan_version: plan.version,
        planner_agent_id: root_agent.id,
        current_agent_id: Some(root_agent.id),
        agent_count: 1,
        review_rounds: 0,
        created_at,
        updated_at: now(),
        last_error: None,
        final_report: None,
    };
    ops.save_task(&task, &plan).await?;
    state.tasks.write().await.insert(
        environment_id,
        Arc::new(TaskRecord {
            summary: RwLock::new(task.clone()),
            plan: RwLock::new(plan),
            plan_history: RwLock::new(Vec::new()),
            reviews: RwLock::new(Vec::new()),
            artifacts: RwLock::new(Vec::new()),
            workflow_lock: Mutex::new(()),
        }),
    );
    Ok(environment_summary(state, &task)
        .await
        .unwrap_or_else(|| environment_summary_from_root(&task, &root_agent, 1)))
}

pub(crate) async fn get_environment(
    state: &RuntimeState,
    ops: &impl EnvironmentOps,
    environment_id: EnvironmentId,
    session_id: Option<SessionId>,
) -> Result<EnvironmentDetail> {
    let task = task(state, environment_id).await?;
    let task_summary = task.summary.read().await.clone();
    let root_agent = ops
        .get_agent(task_summary.planner_agent_id, session_id)
        .await?;
    let summary = environment_summary_from_root(
        &task_summary,
        &root_agent.summary,
        root_agent.sessions.len(),
    );
    let current_conversation_id = root_agent.selected_session_id;
    Ok(EnvironmentDetail {
        summary,
        conversations: root_agent.sessions.clone(),
        current_conversation_id,
        selected_conversation_id: Some(current_conversation_id),
        root_agent,
    })
}

pub(crate) async fn create_environment_conversation(
    state: &RuntimeState,
    ops: &impl EnvironmentOps,
    environment_id: EnvironmentId,
) -> Result<AgentSessionSummary> {
    let task = task(state, environment_id).await?;
    let root_agent_id = task.summary.read().await.planner_agent_id;
    let session = ops.create_agent_session(root_agent_id).await?;
    touch_environment(state, ops, &task).await?;
    Ok(session)
}

pub(crate) async fn send_environment_message(
    state: &RuntimeState,
    ops: &impl EnvironmentOps,
    environment_id: EnvironmentId,
    session_id: SessionId,
    message: String,
    skill_mentions: Vec<String>,
) -> Result<TurnId> {
    let task = task(state, environment_id).await?;
    let root_agent_id = task.summary.read().await.planner_agent_id;
    let turn_id = ops
        .send_agent_message(root_agent_id, session_id, message, skill_mentions)
        .await?;
    touch_environment(state, ops, &task).await?;
    Ok(turn_id)
}

async fn touch_environment(
    state: &RuntimeState,
    ops: &impl EnvironmentOps,
    task: &Arc<TaskRecord>,
) -> Result<()> {
    let plan = task.plan.read().await.clone();
    let updated = {
        let mut summary = task.summary.write().await;
        summary.updated_at = now();
        refresh_summary_counts(state, &mut summary).await;
        summary.clone()
    };
    ops.save_task(&updated, &plan).await?;
    ops.publish_task_updated(updated).await;
    Ok(())
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

pub(crate) async fn environment_summary(
    state: &RuntimeState,
    task: &TaskSummary,
) -> Option<EnvironmentSummary> {
    let root_agent = {
        let agents = state.agents.read().await;
        agents.get(&task.planner_agent_id).cloned()
    }?;
    let root_summary = root_agent.summary.read().await.clone();
    let conversation_count = root_agent.sessions.lock().await.len();
    Some(environment_summary_from_root(
        task,
        &root_summary,
        conversation_count,
    ))
}

fn environment_summary_from_root(
    task: &TaskSummary,
    root_agent: &AgentSummary,
    conversation_count: usize,
) -> EnvironmentSummary {
    EnvironmentSummary {
        id: task.id,
        name: task.title.clone(),
        status: task.status.clone(),
        root_agent_id: task.planner_agent_id,
        conversation_count,
        docker_image: root_agent.docker_image.clone(),
        created_at: task.created_at,
        updated_at: task.updated_at.max(root_agent.updated_at),
        last_error: task
            .last_error
            .clone()
            .or_else(|| root_agent.last_error.clone()),
    }
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

pub(crate) async fn save_task_plan(
    state: &RuntimeState,
    ops: &impl TaskToolOps,
    agent_id: AgentId,
    title: String,
    markdown: String,
) -> Result<TaskSummary> {
    let summary = ops.agent_summary(agent_id).await?;
    if summary.role != Some(AgentRole::Planner) {
        return Err(RuntimeError::InvalidInput(
            "only planner task agents can save task plans".to_string(),
        ));
    }
    let task_id = summary
        .task_id
        .ok_or_else(|| RuntimeError::InvalidInput("agent is not attached to a task".to_string()))?;
    let task = task(state, task_id).await?;
    {
        let mut plan = task.plan.write().await;
        if plan.version > 0 {
            let entry = PlanHistoryEntry {
                version: plan.version,
                title: plan.title.clone(),
                markdown: plan.markdown.clone(),
                saved_at: plan.saved_at,
                saved_by_agent_id: plan.saved_by_agent_id,
                revision_feedback: plan.revision_feedback.clone(),
                revision_requested_at: plan.revision_requested_at,
            };
            ops.save_plan_history_entry(task_id, &entry).await?;
            task.plan_history.write().await.push(entry);
        }
        let version = plan.version.saturating_add(1).max(1);
        *plan = TaskPlan {
            status: PlanStatus::Ready,
            title: Some(title.trim().to_string()),
            markdown: Some(markdown.trim().to_string()),
            version,
            saved_by_agent_id: Some(agent_id),
            saved_at: Some(now()),
            approved_at: None,
            revision_feedback: None,
            revision_requested_at: None,
        };
        let updated = {
            let mut task_summary = task.summary.write().await;
            task_summary.status = TaskStatus::AwaitingApproval;
            task_summary.plan_status = PlanStatus::Ready;
            task_summary.plan_version = version;
            task_summary.current_agent_id = Some(agent_id);
            task_summary.updated_at = now();
            refresh_summary_counts(state, &mut task_summary).await;
            task_summary.clone()
        };
        ops.save_task(&updated, &plan).await?;
        ops.publish_plan_updated(task_id, plan.clone()).await;
        ops.publish_task_updated(updated).await;
    }
    Ok(task_summary(state, &task).await)
}

pub(crate) async fn submit_review_result(
    state: &RuntimeState,
    ops: &impl TaskToolOps,
    agent_id: AgentId,
    passed: bool,
    findings: String,
    summary: String,
) -> Result<TaskReview> {
    let agent_summary = ops.agent_summary(agent_id).await?;
    if agent_summary.role != Some(AgentRole::Reviewer) {
        return Err(RuntimeError::InvalidInput(
            "only reviewer task agents can submit review results".to_string(),
        ));
    }
    let task_id = agent_summary
        .task_id
        .ok_or_else(|| RuntimeError::InvalidInput("agent is not attached to a task".to_string()))?;
    let task = task(state, task_id).await?;
    let review = {
        let mut reviews = task.reviews.write().await;
        let review = TaskReview {
            id: Uuid::new_v4(),
            task_id,
            reviewer_agent_id: agent_id,
            round: reviews.len() as u64 + 1,
            passed,
            findings,
            summary,
            created_at: now(),
        };
        ops.append_task_review(&review).await?;
        reviews.push(review.clone());
        review
    };
    {
        let plan = task.plan.read().await.clone();
        let updated = {
            let mut summary = task.summary.write().await;
            summary.review_rounds = task.reviews.read().await.len() as u64;
            summary.updated_at = now();
            refresh_summary_counts(state, &mut summary).await;
            summary.clone()
        };
        ops.save_task(&updated, &plan).await?;
        ops.publish_task_updated(updated).await;
    }
    Ok(review)
}

fn sync_plan_fields(summary: &mut TaskSummary, plan: &TaskPlan) {
    summary.plan_status = plan.status.clone();
    summary.plan_version = plan.version;
    if plan.status == PlanStatus::Missing {
        summary.plan_version = 0;
    }
}
