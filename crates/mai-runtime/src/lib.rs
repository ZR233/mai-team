use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::Utc;
use mai_docker::{ContainerHandle, DockerClient};
use mai_mcp::McpAgentManager;
use mai_model::ResponsesClient;
use mai_protocol::{
    AgentConfigRequest, AgentConfigResponse, AgentDetail, AgentId, AgentMessage,
    AgentModelPreference, AgentRole, AgentSessionSummary, AgentStatus, AgentSummary, ArtifactInfo,
    ContextUsage, CreateAgentRequest, MessageRole, ModelConfig, ModelContentItem, ModelInputItem,
    ModelOutputItem, ModelToolCall, PlanHistoryEntry, PlanStatus, ResolvedAgentModelPreference,
    ServiceEvent, ServiceEventKind, SessionId, TaskDetail, TaskId, TaskPlan, TaskReview, TaskStatus,
    TaskSummary, TokenUsage, TodoItem, ToolTraceDetail, TurnId, TurnStatus, UpdateAgentRequest,
    UserInputOption, UserInputQuestion, now, preview,
};
use mai_skills::SkillsManager;
use mai_store::{ConfigStore, ProviderSelection};
use mai_tools::{RoutedTool, build_tool_definitions, route_tool};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio::time::{Duration, Instant, sleep};
use uuid::Uuid;

const MAX_TOOL_ITERATIONS: usize = 16;
const RECENT_EVENT_LIMIT: usize = 500;
const AUTO_COMPACT_THRESHOLD_PERCENT: u64 = 80;
const REVIEW_ROUND_LIMIT: u64 = 5;
const COMPACT_USER_MESSAGE_MAX_CHARS: usize = 80_000;
const COMPACT_SUMMARY_PREVIEW_CHARS: usize = 240;
const COMPACT_SUMMARY_PREFIX: &str = "Context checkpoint summary from earlier conversation history. This is background for continuity, not a new user request.";
const COMPACT_PROMPT: &str = r#"You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will continue this agent session.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done as clear next steps
- Any critical data, examples, file paths, command outputs, or references needed to continue

Be concise, structured, and focused on helping the next model seamlessly continue the work."#;
const AGENT_ROLES: [AgentRole; 4] = [
    AgentRole::Planner,
    AgentRole::Explorer,
    AgentRole::Executor,
    AgentRole::Reviewer,
];

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("agent not found: {0}")]
    AgentNotFound(AgentId),
    #[error("task not found: {0}")]
    TaskNotFound(TaskId),
    #[error("agent is busy: {0}")]
    AgentBusy(AgentId),
    #[error("task is busy: {0}")]
    TaskBusy(TaskId),
    #[error("agent has no container: {0}")]
    MissingContainer(AgentId),
    #[error("session not found: {agent_id}/{session_id}")]
    SessionNotFound {
        agent_id: AgentId,
        session_id: SessionId,
    },
    #[error("tool trace not found: {agent_id}/{call_id}")]
    ToolTraceNotFound { agent_id: AgentId, call_id: String },
    #[error("docker error: {0}")]
    Docker(#[from] mai_docker::DockerError),
    #[error("model error: {0}")]
    Model(#[from] mai_model::ModelError),
    #[error("mcp error: {0}")]
    Mcp(#[from] mai_mcp::McpError),
    #[error("store error: {0}")]
    Store(#[from] mai_store::StoreError),
    #[error("skill error: {0}")]
    Skill(#[from] mai_skills::SkillError),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

#[derive(Clone)]
pub struct RuntimeConfig {
    pub repo_root: PathBuf,
}

pub struct AgentRuntime {
    docker: DockerClient,
    model: ResponsesClient,
    store: Arc<ConfigStore>,
    skills: SkillsManager,
    agents: RwLock<HashMap<AgentId, Arc<AgentRecord>>>,
    tasks: RwLock<HashMap<TaskId, Arc<TaskRecord>>>,
    event_tx: broadcast::Sender<ServiceEvent>,
    sequence: AtomicU64,
    recent_events: Mutex<VecDeque<ServiceEvent>>,
    repo_root: PathBuf,
}

struct TaskRecord {
    summary: RwLock<TaskSummary>,
    plan: RwLock<TaskPlan>,
    plan_history: RwLock<Vec<PlanHistoryEntry>>,
    reviews: RwLock<Vec<TaskReview>>,
    artifacts: RwLock<Vec<ArtifactInfo>>,
    workflow_lock: Mutex<()>,
}

struct AgentRecord {
    summary: RwLock<AgentSummary>,
    sessions: Mutex<Vec<AgentSessionRecord>>,
    container: RwLock<Option<ContainerHandle>>,
    mcp: RwLock<Option<Arc<McpAgentManager>>>,
    system_prompt: Option<String>,
    turn_lock: Mutex<()>,
    cancel_requested: AtomicBool,
}

#[derive(Clone)]
struct AgentSessionRecord {
    summary: AgentSessionSummary,
    messages: Vec<AgentMessage>,
    history: Vec<ModelInputItem>,
    last_context_tokens: Option<u64>,
    last_turn_response: Option<String>,
}

#[derive(Debug)]
struct ToolExecution {
    success: bool,
    output: String,
    ends_turn: bool,
}

#[derive(Debug, Clone)]
enum ContainerSource {
    FreshImage,
    CloneFrom {
        parent_container_id: String,
        docker_image: String,
    },
}

struct ResolvedAgentModel {
    preference: AgentModelPreference,
    effective: ResolvedAgentModelPreference,
}

impl AgentRuntime {
    pub async fn new(
        docker: DockerClient,
        model: ResponsesClient,
        store: Arc<ConfigStore>,
        config: RuntimeConfig,
    ) -> Result<Arc<Self>> {
        let skills = SkillsManager::new(&config.repo_root);
        let (event_tx, _) = broadcast::channel(1024);
        let snapshot = store.load_runtime_snapshot(RECENT_EVENT_LIMIT).await?;
        let mut agents = HashMap::new();
        for persisted in snapshot.agents {
            let (summary, changed) = recovered_summary(persisted.summary);
            let mut sessions = Vec::new();
            for persisted_session in persisted.sessions {
                let messages = store
                    .load_agent_messages(summary.id, persisted_session.summary.id)
                    .await?;
                sessions.push(AgentSessionRecord {
                    summary: AgentSessionSummary {
                        message_count: messages.len(),
                        ..persisted_session.summary
                    },
                    messages,
                    history: persisted_session.history,
                    last_context_tokens: persisted_session.last_context_tokens,
                    last_turn_response: None,
                });
            }
            if sessions.is_empty() {
                sessions.push(default_session_record());
            }
            let agent = Arc::new(AgentRecord {
                summary: RwLock::new(summary.clone()),
                sessions: Mutex::new(sessions),
                container: RwLock::new(None),
                mcp: RwLock::new(None),
                system_prompt: persisted.system_prompt,
                turn_lock: Mutex::new(()),
                cancel_requested: AtomicBool::new(false),
            });
            if changed {
                store
                    .save_agent(&summary, agent.system_prompt.as_deref())
                    .await?;
            }
            agents.insert(summary.id, agent);
        }
        let mut tasks = HashMap::new();
        for persisted in snapshot.tasks {
            let mut summary = persisted.summary;
            let mut agent_count = 0;
            for agent in agents.values() {
                if agent.summary.read().await.task_id == Some(summary.id) {
                    agent_count += 1;
                }
            }
            summary.agent_count = agent_count;
            summary.review_rounds = persisted.reviews.len() as u64;
            let task = Arc::new(TaskRecord {
                summary: RwLock::new(summary.clone()),
                plan: RwLock::new(persisted.plan),
                plan_history: RwLock::new(persisted.plan_history),
                reviews: RwLock::new(persisted.reviews),
                artifacts: RwLock::new(persisted.artifacts),
                workflow_lock: Mutex::new(()),
            });
            tasks.insert(summary.id, task);
        }

        Ok(Arc::new(Self {
            docker,
            model,
            store,
            skills,
            agents: RwLock::new(agents),
            tasks: RwLock::new(tasks),
            event_tx,
            sequence: AtomicU64::new(snapshot.next_sequence),
            recent_events: Mutex::new(snapshot.recent_events.into_iter().collect()),
            repo_root: config.repo_root,
        }))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        self.event_tx.subscribe()
    }

    pub async fn agent_config(&self) -> Result<AgentConfigResponse> {
        let config = self.store.load_agent_config().await?;
        let planner = role_preference(&config, AgentRole::Planner).cloned();
        let explorer = role_preference(&config, AgentRole::Explorer).cloned();
        let executor = role_preference(&config, AgentRole::Executor).cloned();
        let reviewer = role_preference(&config, AgentRole::Reviewer).cloned();
        let mut validation_errors = Vec::new();
        let effective_planner = self
            .resolve_effective_agent_model(
                AgentRole::Planner,
                planner.as_ref(),
                &mut validation_errors,
            )
            .await;
        let effective_explorer = self
            .resolve_effective_agent_model(
                AgentRole::Explorer,
                explorer.as_ref(),
                &mut validation_errors,
            )
            .await;
        let effective_executor = self
            .resolve_effective_agent_model(
                AgentRole::Executor,
                executor.as_ref(),
                &mut validation_errors,
            )
            .await;
        let effective_reviewer = self
            .resolve_effective_agent_model(
                AgentRole::Reviewer,
                reviewer.as_ref(),
                &mut validation_errors,
            )
            .await;
        let validation_error =
            (!validation_errors.is_empty()).then(|| validation_errors.join("; "));
        Ok(AgentConfigResponse {
            planner,
            explorer,
            executor,
            reviewer,
            effective_planner,
            effective_explorer,
            effective_executor,
            effective_reviewer,
            validation_error,
        })
    }

    pub async fn update_agent_config(
        &self,
        request: AgentConfigRequest,
    ) -> Result<AgentConfigResponse> {
        for role in AGENT_ROLES {
            let preference = role_preference(&request, role);
            self.resolve_agent_model_preference(role, preference)
                .await?;
        }
        self.store.save_agent_config(&request).await?;
        self.agent_config().await
    }

    pub async fn list_tasks(&self) -> Vec<TaskSummary> {
        let task_records = {
            let tasks = self.tasks.read().await;
            tasks.values().cloned().collect::<Vec<_>>()
        };
        let mut summaries = Vec::with_capacity(task_records.len());
        for task in task_records {
            let mut summary = task.summary.read().await.clone();
            self.refresh_task_summary_counts(&mut summary).await;
            summaries.push(summary);
        }
        summaries.sort_by_key(|summary| summary.created_at);
        summaries
    }

    pub async fn ensure_default_task(self: &Arc<Self>) -> Result<Option<TaskSummary>> {
        let tasks = self.list_tasks().await;
        if let Some(task) = tasks.first() {
            return Ok(Some(task.clone()));
        }
        match self.create_task(None, None, None).await {
            Ok(task) => Ok(Some(task)),
            Err(RuntimeError::Store(mai_store::StoreError::InvalidConfig(_))) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn create_task(
        self: &Arc<Self>,
        title: Option<String>,
        initial_message: Option<String>,
        docker_image: Option<String>,
    ) -> Result<TaskSummary> {
        let task_id = Uuid::new_v4();
        let user_omitted_title = title
            .as_ref()
            .map(|v| v.trim().is_empty())
            .unwrap_or(true);
        let title = title
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "New Task".to_string());
        let planner_model = self.resolve_role_agent_model(AgentRole::Planner).await?;
        let created_at = now();
        let planner = self
            .create_agent_with_container_source(
                CreateAgentRequest {
                    name: Some(format!("{title} Planner")),
                    provider_id: Some(planner_model.preference.provider_id),
                    model: Some(planner_model.preference.model),
                    reasoning_effort: planner_model.preference.reasoning_effort,
                    docker_image,
                    parent_id: None,
                    system_prompt: Some(task_role_system_prompt(AgentRole::Planner).to_string()),
                },
                ContainerSource::FreshImage,
                Some(task_id),
                Some(AgentRole::Planner),
            )
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
        self.store.save_task(&summary, &plan).await?;
        let task = Arc::new(TaskRecord {
            summary: RwLock::new(summary.clone()),
            plan: RwLock::new(plan),
            plan_history: RwLock::new(Vec::new()),
            reviews: RwLock::new(Vec::new()),
            artifacts: RwLock::new(Vec::new()),
            workflow_lock: Mutex::new(()),
        });
        self.tasks.write().await.insert(task_id, task);
        self.publish(ServiceEventKind::TaskCreated {
            task: summary.clone(),
        })
        .await;
        let message_for_title = initial_message
            .as_ref()
            .filter(|m| !m.trim().is_empty())
            .cloned();
        if let Some(message) = initial_message.filter(|message| !message.trim().is_empty()) {
            let _ = self
                .send_task_message(task_id, message, Vec::new())
                .await?;
        }
        if user_omitted_title {
            if let Some(message_text) = message_for_title {
                let runtime = Arc::clone(&self);
                tokio::spawn(async move {
                    match runtime.generate_task_title(&message_text).await {
                        Ok(new_title) => {
                            if let Err(err) = runtime.update_task_title(task_id, new_title).await {
                                tracing::warn!("failed to update task title: {err}");
                            }
                        }
                        Err(err) => {
                            tracing::warn!("failed to generate task title: {err}");
                        }
                    }
                });
            }
        }
        Ok(summary)
    }

    async fn generate_task_title(self: &Arc<Self>, message: &str) -> Result<String> {
        let planner_model = self.resolve_role_agent_model(AgentRole::Planner).await?;
        let selection = self
            .store
            .resolve_provider(
                Some(&planner_model.preference.provider_id),
                Some(&planner_model.preference.model),
            )
            .await?;
        let instructions = "Generate a concise task title of 3-8 words that captures the essence of the user's request. Output only the title text, nothing else. Do not use quotes or punctuation at the end.";
        let input = vec![ModelInputItem::user_text(message)];
        let response = self
            .model
            .create_response(&selection.provider, &selection.model, instructions, &input, &[], None)
            .await?;
        let title = response
            .output
            .into_iter()
            .filter_map(|item| match item {
                ModelOutputItem::Message { text } => Some(text),
                ModelOutputItem::AssistantTurn { content, .. } => content,
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
            .trim()
            .to_string();
        if title.is_empty() {
            return Ok("New Task".to_string());
        }
        let title = if title.len() > 100 {
            title.chars().take(100).collect()
        } else {
            title
        };
        Ok(title)
    }

    async fn update_task_title(self: &Arc<Self>, task_id: TaskId, new_title: String) -> Result<()> {
        let task = self.task(task_id).await?;
        let plan = task.plan.read().await.clone();
        {
            let mut summary = task.summary.write().await;
            summary.title = new_title;
            summary.updated_at = now();
            self.refresh_task_summary_counts(&mut summary).await;
            self.store.save_task(&summary, &plan).await?;
            self.publish(ServiceEventKind::TaskUpdated {
                task: summary.clone(),
            })
            .await;
        }
        Ok(())
    }

    pub async fn get_task(
        &self,
        task_id: TaskId,
        selected_agent_id: Option<AgentId>,
    ) -> Result<TaskDetail> {
        let task = self.task(task_id).await?;
        let summary = self.task_summary(&task).await;
        let plan = task.plan.read().await.clone();
        let plan_history = task.plan_history.read().await.clone();
        let reviews = task.reviews.read().await.clone();
        let agents = self.task_agents(task_id).await;
        let selected_agent_id = selected_agent_id
            .filter(|id| agents.iter().any(|agent| agent.id == *id))
            .or(summary.current_agent_id)
            .unwrap_or(summary.planner_agent_id);
        let selected_agent = self.get_agent(selected_agent_id, None).await?;
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

    pub async fn send_task_message(
        self: &Arc<Self>,
        task_id: TaskId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        let task = self.task(task_id).await?;
        let planner_agent_id = task.summary.read().await.planner_agent_id;
        {
            let mut plan = task.plan.write().await;
            if plan.status == PlanStatus::Ready || plan.status == PlanStatus::Approved {
                let entry = PlanHistoryEntry {
                    version: plan.version,
                    title: plan.title.clone(),
                    markdown: plan.markdown.clone(),
                    saved_at: plan.saved_at,
                    saved_by_agent_id: plan.saved_by_agent_id,
                    revision_feedback: None,
                    revision_requested_at: None,
                };
                self.store.save_plan_history_entry(task_id, &entry).await?;
                task.plan_history.write().await.push(entry);
                plan.status = PlanStatus::NeedsRevision;
                plan.revision_feedback = None;
                plan.revision_requested_at = None;
                plan.approved_at = None;
                let mut summary = task.summary.write().await;
                summary.status = TaskStatus::Planning;
                summary.plan_status = PlanStatus::NeedsRevision;
                summary.final_report = None;
                summary.last_error = None;
                summary.updated_at = now();
                self.store.save_task(&summary, &plan).await?;
                self.publish(ServiceEventKind::PlanUpdated {
                    task_id,
                    plan: plan.clone(),
                })
                .await;
                self.publish(ServiceEventKind::TaskUpdated {
                    task: summary.clone(),
                })
                .await;
            }
        }
        let turn_id = self
            .send_message(planner_agent_id, None, message, skill_mentions)
            .await?;
        self.set_task_current_agent(&task, planner_agent_id, TaskStatus::Planning, None)
            .await?;
        Ok(turn_id)
    }

    pub async fn approve_task_plan(self: &Arc<Self>, task_id: TaskId) -> Result<TaskSummary> {
        let task = self.task(task_id).await?;
        {
            let mut plan = task.plan.write().await;
            if plan.status != PlanStatus::Ready || plan.markdown.as_deref().unwrap_or("").is_empty()
            {
                return Err(RuntimeError::InvalidInput(
                    "task has no ready plan to approve".to_string(),
                ));
            }
            plan.status = PlanStatus::Approved;
            plan.approved_at = Some(now());
            let mut summary = task.summary.write().await;
            summary.status = TaskStatus::Executing;
            summary.plan_status = PlanStatus::Approved;
            summary.plan_version = plan.version;
            summary.updated_at = now();
            self.store.save_task(&summary, &plan).await?;
            self.publish(ServiceEventKind::TaskUpdated {
                task: summary.clone(),
            })
            .await;
        }
        self.spawn_task_workflow(task_id);
        Ok(self.task_summary(&task).await)
    }

    pub async fn request_plan_revision(
        self: &Arc<Self>,
        task_id: TaskId,
        feedback: String,
    ) -> Result<TaskSummary> {
        let task = self.task(task_id).await?;
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
            self.store.save_plan_history_entry(task_id, &entry).await?;
            task.plan_history.write().await.push(entry);
            plan.status = PlanStatus::NeedsRevision;
            plan.revision_feedback = Some(feedback.clone());
            plan.revision_requested_at = Some(now());
            let mut summary = task.summary.write().await;
            summary.status = TaskStatus::Planning;
            summary.plan_status = PlanStatus::NeedsRevision;
            summary.updated_at = now();
            self.store.save_task(&summary, &plan).await?;
            self.publish(ServiceEventKind::PlanUpdated {
                task_id,
                plan: plan.clone(),
            })
            .await;
            self.publish(ServiceEventKind::TaskUpdated {
                task: summary.clone(),
            })
            .await;
        }
        let planner_agent_id = task.summary.read().await.planner_agent_id;
        let feedback_message = format!(
            "The user requests revision of the plan.\n\nFeedback:\n{feedback}\n\nPlease address the feedback and save an updated plan."
        );
        let _ = self
            .send_message(planner_agent_id, None, feedback_message, Vec::new())
            .await?;
        self.set_task_current_agent(&task, planner_agent_id, TaskStatus::Planning, None)
            .await?;
        Ok(self.task_summary(&task).await)
    }

    pub async fn create_agent(
        self: &Arc<Self>,
        request: CreateAgentRequest,
    ) -> Result<AgentSummary> {
        self.create_agent_with_container_source(request, ContainerSource::FreshImage, None, None)
            .await
    }

    async fn create_agent_with_container_source(
        self: &Arc<Self>,
        request: CreateAgentRequest,
        container_source: ContainerSource,
        task_id: Option<TaskId>,
        role: Option<AgentRole>,
    ) -> Result<AgentSummary> {
        let id = Uuid::new_v4();
        let created_at = Utc::now();
        let name = request
            .name
            .unwrap_or_else(|| format!("agent-{}", short_id(id)));
        let provider_selection = self
            .store
            .resolve_provider(request.provider_id.as_deref(), request.model.as_deref())
            .await?;
        let reasoning_effort = normalize_reasoning_effort(
            &provider_selection.model,
            request.reasoning_effort.as_deref(),
            true,
        )?;
        let docker_image = self.resolve_docker_image(request.docker_image.as_deref());
        let system_prompt = request.system_prompt;
        let summary = AgentSummary {
            id,
            parent_id: request.parent_id,
            task_id,
            role,
            name,
            status: AgentStatus::Created,
            container_id: None,
            docker_image,
            provider_id: provider_selection.provider.id.clone(),
            provider_name: provider_selection.provider.name.clone(),
            model: provider_selection.model.id.clone(),
            reasoning_effort,
            created_at,
            updated_at: created_at,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        };
        self.store
            .save_agent(&summary, system_prompt.as_deref())
            .await?;
        let session = if task_id.is_some() {
            session_record_with_title("Task")
        } else {
            default_session_record()
        };
        self.store.save_agent_session(id, &session.summary).await?;

        let agent = Arc::new(AgentRecord {
            summary: RwLock::new(summary.clone()),
            sessions: Mutex::new(vec![session]),
            container: RwLock::new(None),
            mcp: RwLock::new(None),
            system_prompt,
            turn_lock: Mutex::new(()),
            cancel_requested: AtomicBool::new(false),
        });

        self.agents.write().await.insert(id, Arc::clone(&agent));
        self.publish(ServiceEventKind::AgentCreated {
            agent: summary.clone(),
        })
        .await;

        match self
            .ensure_agent_container_with_source(&agent, AgentStatus::Idle, &container_source)
            .await
        {
            Ok(_) => Ok(agent.summary.read().await.clone()),
            Err(err) => {
                let message = err.to_string();
                if let Err(store_err) = self
                    .set_status(&agent, AgentStatus::Failed, Some(message.clone()))
                    .await
                {
                    tracing::warn!("failed to persist agent failure: {store_err}");
                }
                self.publish(ServiceEventKind::Error {
                    agent_id: Some(id),
                    session_id: None,
                    turn_id: None,
                    message,
                })
                .await;
                Err(err)
            }
        }
    }

    pub async fn list_agents(&self) -> Vec<AgentSummary> {
        let agents = self.agents.read().await;
        let mut summaries = Vec::with_capacity(agents.len());
        for agent in agents.values() {
            summaries.push(agent.summary.read().await.clone());
        }
        summaries.sort_by_key(|s| s.created_at);
        summaries
    }

    pub async fn update_agent(
        &self,
        agent_id: AgentId,
        request: UpdateAgentRequest,
    ) -> Result<AgentSummary> {
        let agent = self.agent(agent_id).await?;
        {
            let summary = agent.summary.read().await;
            if !summary.status.can_start_turn() || summary.current_turn.is_some() {
                return Err(RuntimeError::AgentBusy(agent_id));
            }
        }
        let current = agent.summary.read().await.clone();
        let provider_id = request
            .provider_id
            .as_deref()
            .or(Some(&current.provider_id));
        let model = request.model.as_deref().or(Some(&current.model));
        let provider_selection = self.store.resolve_provider(provider_id, model).await?;
        let requested_reasoning_effort = if request.reasoning_effort.is_some()
            || provider_selection.model.id != current.model
            || provider_selection.provider.id != current.provider_id
        {
            request.reasoning_effort
        } else {
            current.reasoning_effort
        };
        let reasoning_effort = normalize_reasoning_effort(
            &provider_selection.model,
            requested_reasoning_effort.as_deref(),
            true,
        )?;
        let updated = {
            let mut summary = agent.summary.write().await;
            summary.provider_id = provider_selection.provider.id.clone();
            summary.provider_name = provider_selection.provider.name.clone();
            summary.model = provider_selection.model.id.clone();
            summary.reasoning_effort = reasoning_effort;
            summary.updated_at = now();
            summary.clone()
        };
        self.persist_agent(&agent).await?;
        self.publish(ServiceEventKind::AgentUpdated {
            agent: updated.clone(),
        })
        .await;
        Ok(updated)
    }

    pub async fn cleanup_orphaned_containers(&self) -> Result<Vec<String>> {
        let active_agent_ids = {
            let agents = self.agents.read().await;
            agents
                .keys()
                .map(ToString::to_string)
                .collect::<HashSet<_>>()
        };
        Ok(self
            .docker
            .cleanup_orphaned_agent_containers(&active_agent_ids)
            .await?)
    }

    pub async fn get_agent(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> Result<AgentDetail> {
        let agent = self.agent(agent_id).await?;
        let summary = agent.summary.read().await.clone();
        let (sessions, selected_session_id, context_tokens_used, messages) = {
            let sessions = agent.sessions.lock().await;
            let selected_session = selected_session(&sessions, session_id).ok_or_else(|| {
                RuntimeError::SessionNotFound {
                    agent_id,
                    session_id: session_id.unwrap_or_default(),
                }
            })?;
            (
                sessions
                    .iter()
                    .map(|session| session.summary.clone())
                    .collect(),
                selected_session.summary.id,
                selected_session.last_context_tokens.unwrap_or_default(),
                selected_session.messages.clone(),
            )
        };
        let context_usage = self
            .store
            .resolve_provider(Some(&summary.provider_id), Some(&summary.model))
            .await
            .ok()
            .map(|provider_selection| ContextUsage {
                used_tokens: context_tokens_used,
                context_tokens: provider_selection.model.context_tokens,
                threshold_percent: AUTO_COMPACT_THRESHOLD_PERCENT,
            });
        let recent_events = self
            .recent_events
            .lock()
            .await
            .iter()
            .filter(|event| event_agent_id(event) == Some(agent_id))
            .cloned()
            .collect();
        Ok(AgentDetail {
            summary,
            sessions,
            selected_session_id,
            context_usage,
            messages,
            recent_events,
        })
    }

    pub async fn create_session(&self, agent_id: AgentId) -> Result<AgentSessionSummary> {
        let agent = self.agent(agent_id).await?;
        if agent.summary.read().await.task_id.is_some() {
            return Err(RuntimeError::InvalidInput(
                "task-owned agents use a single internal task session".to_string(),
            ));
        }
        let session = {
            let mut sessions = agent.sessions.lock().await;
            let session = AgentSessionRecord {
                summary: AgentSessionSummary {
                    id: Uuid::new_v4(),
                    title: format!("Chat {}", sessions.len() + 1),
                    created_at: now(),
                    updated_at: now(),
                    message_count: 0,
                },
                messages: Vec::new(),
                history: Vec::new(),
                last_context_tokens: None,
                last_turn_response: None,
            };
            sessions.push(session.clone());
            session.summary
        };
        self.store.save_agent_session(agent_id, &session).await?;
        Ok(session)
    }

    pub async fn tool_trace(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        call_id: String,
    ) -> Result<ToolTraceDetail> {
        let agent = self.agent(agent_id).await?;
        let (session_id, history) = {
            let sessions = agent.sessions.lock().await;
            let selected_session = selected_session(&sessions, session_id).ok_or_else(|| {
                RuntimeError::SessionNotFound {
                    agent_id,
                    session_id: session_id.unwrap_or_default(),
                }
            })?;
            (
                selected_session.summary.id,
                selected_session.history.clone(),
            )
        };
        let mut tool_name = None;
        let mut arguments = None;
        let mut output = None;

        for item in history {
            match item {
                ModelInputItem::FunctionCall {
                    call_id: item_call_id,
                    name,
                    arguments: raw_arguments,
                } if item_call_id == call_id => {
                    tool_name = Some(name);
                    arguments = Some(parse_tool_arguments(&raw_arguments));
                }
                ModelInputItem::AssistantTurn { tool_calls, .. } => {
                    for tool_call in tool_calls {
                        if tool_call.call_id == call_id {
                            tool_name = Some(tool_call.name);
                            arguments = Some(parse_tool_arguments(&tool_call.arguments));
                            break;
                        }
                    }
                }
                ModelInputItem::FunctionCallOutput {
                    call_id: item_call_id,
                    output: item_output,
                } if item_call_id == call_id => {
                    output = Some(item_output);
                }
                _ => {}
            }
        }

        let tool_name = tool_name.ok_or_else(|| RuntimeError::ToolTraceNotFound {
            agent_id,
            call_id: call_id.clone(),
        })?;
        let output = output.unwrap_or_default();
        let (event_success, duration_ms) = self
            .tool_event_metadata(agent_id, session_id, &call_id)
            .await;
        Ok(ToolTraceDetail {
            call_id,
            tool_name,
            arguments: arguments.unwrap_or_else(|| json!({})),
            success: event_success.unwrap_or(!output.is_empty()),
            output,
            duration_ms,
        })
    }

    pub async fn send_message(
        self: &Arc<Self>,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        let session_id = self.resolve_session_id(agent_id, session_id).await?;
        let turn_id = self.prepare_turn(agent_id).await?;
        self.spawn_turn(agent_id, session_id, turn_id, message, skill_mentions);
        Ok(turn_id)
    }

    async fn prepare_turn(&self, agent_id: AgentId) -> Result<TurnId> {
        let agent = self.agent(agent_id).await?;
        let turn_id = Uuid::new_v4();
        let should_start = {
            let mut summary = agent.summary.write().await;
            if !summary.status.can_start_turn() {
                false
            } else {
                summary.status = AgentStatus::RunningTurn;
                summary.current_turn = Some(turn_id);
                summary.updated_at = now();
                summary.last_error = None;
                agent.cancel_requested.store(false, Ordering::SeqCst);
                true
            }
        };
        if !should_start {
            return Err(RuntimeError::AgentBusy(agent_id));
        }
        self.persist_agent(&agent).await?;
        self.publish(ServiceEventKind::AgentStatusChanged {
            agent_id,
            status: AgentStatus::RunningTurn,
        })
        .await;
        Ok(turn_id)
    }

    fn spawn_turn(
        self: &Arc<Self>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
    ) {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            runtime
                .run_turn(agent_id, session_id, turn_id, message, skill_mentions)
                .await;
        });
    }

    pub async fn cancel_agent(&self, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        agent.cancel_requested.store(true, Ordering::SeqCst);
        self.set_status(&agent, AgentStatus::Cancelled, None)
            .await?;
        Ok(())
    }

    pub async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
        let targets = self.descendant_delete_order(agent_id).await?;
        for target_id in targets {
            self.delete_agent_record(target_id).await?;
        }
        Ok(())
    }

    pub async fn cancel_task(&self, task_id: TaskId) -> Result<()> {
        let task = self.task(task_id).await?;
        let agents = self.task_agents(task_id).await;
        for agent in agents {
            if let Ok(record) = self.agent(agent.id).await {
                record.cancel_requested.store(true, Ordering::SeqCst);
                let _ = self.set_status(&record, AgentStatus::Cancelled, None).await;
            }
        }
        self.set_task_status(&task, TaskStatus::Cancelled, None, None)
            .await?;
        Ok(())
    }

    pub async fn delete_task(self: &Arc<Self>, task_id: TaskId) -> Result<()> {
        let _task = self.task(task_id).await?;
        let root_agents = self
            .task_agents(task_id)
            .await
            .into_iter()
            .filter(|agent| agent.parent_id.is_none())
            .map(|agent| agent.id)
            .collect::<Vec<_>>();
        for agent_id in root_agents {
            let _ = self.delete_agent(agent_id).await;
        }
        self.store.delete_task(task_id).await?;
        self.tasks.write().await.remove(&task_id);
        self.publish(ServiceEventKind::TaskDeleted { task_id }).await;
        Ok(())
    }

    async fn delete_agent_record(&self, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        agent.cancel_requested.store(true, Ordering::SeqCst);
        self.set_status(&agent, AgentStatus::DeletingContainer, None)
            .await?;
        *agent.mcp.write().await = None;
        let in_memory_container_id = agent
            .container
            .write()
            .await
            .take()
            .map(|container| container.id);
        let persisted_container_id = agent.summary.read().await.container_id.clone();
        let preferred_container_id = in_memory_container_id.or(persisted_container_id);
        let deleted = self
            .docker
            .delete_agent_containers(&agent_id.to_string(), preferred_container_id.as_deref())
            .await?;
        if !deleted.is_empty() {
            tracing::info!(
                agent_id = %agent_id,
                count = deleted.len(),
                "removed agent containers"
            );
        }
        let _turn_guard = agent.turn_lock.lock().await;
        self.set_status(&agent, AgentStatus::Deleted, None).await?;
        self.store.delete_agent(agent_id).await?;
        self.agents.write().await.remove(&agent_id);
        self.publish(ServiceEventKind::AgentDeleted { agent_id })
            .await;
        Ok(())
    }

    async fn descendant_delete_order(&self, root_id: AgentId) -> Result<Vec<AgentId>> {
        let summaries = {
            let agents = self.agents.read().await;
            let mut summaries = Vec::with_capacity(agents.len());
            for agent in agents.values() {
                summaries.push(agent.summary.read().await.clone());
            }
            summaries
        };
        if !summaries.iter().any(|summary| summary.id == root_id) {
            return Err(RuntimeError::AgentNotFound(root_id));
        }

        Ok(descendant_delete_order_from_summaries(root_id, &summaries))
    }

    pub async fn upload_file(
        &self,
        agent_id: AgentId,
        path: String,
        content_base64: String,
    ) -> Result<usize> {
        let bytes = BASE64
            .decode(content_base64.trim())
            .map_err(|err| RuntimeError::InvalidInput(format!("invalid base64: {err}")))?;
        let temp = NamedTempFile::new()?;
        std::fs::write(temp.path(), &bytes)?;
        let container_id = self.container_id(agent_id).await?;
        self.docker
            .copy_to_container(&container_id, temp.path(), &path)
            .await?;
        Ok(bytes.len())
    }

    pub async fn download_file_tar(&self, agent_id: AgentId, path: String) -> Result<Vec<u8>> {
        let container_id = self.container_id(agent_id).await?;
        Ok(self
            .docker
            .copy_from_container_tar(&container_id, &path)
            .await?)
    }

    pub async fn save_artifact(
        self: &Arc<Self>,
        agent_id: AgentId,
        path: String,
        display_name: Option<String>,
    ) -> Result<ArtifactInfo> {
        let agent = self.agent(agent_id).await?;
        let task_id = agent.summary.read().await.task_id.ok_or_else(|| {
            RuntimeError::InvalidInput("Agent has no task".to_string())
        })?;
        let container_id = self.container_id(agent_id).await?;

        let name = display_name.unwrap_or_else(|| {
            Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone())
        });

        let artifact_id = Uuid::new_v4().to_string();
        let dir = self.repo_root.join("artifacts").join(task_id.to_string()).join(&artifact_id);
        std::fs::create_dir_all(&dir)?;

        let dest = dir.join(&name);
        self.docker
            .copy_from_container_to_file(&container_id, &path, &dest)
            .await?;

        let size_bytes = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);

        let info = ArtifactInfo {
            id: artifact_id,
            agent_id,
            task_id,
            name,
            path,
            size_bytes,
            created_at: Utc::now(),
        };

        self.store.save_artifact(&info)?;

        let task = self.task(task_id).await?;
        task.artifacts.write().await.push(info.clone());

        self.publish(ServiceEventKind::ArtifactCreated {
            artifact: info.clone(),
        })
        .await;

        Ok(info)
    }

    async fn run_turn(
        self: Arc<Self>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
    ) {
        let result = self
            .run_turn_inner(agent_id, session_id, turn_id, message, skill_mentions)
            .await;
        if let Err(err) = result
            && let Ok(agent) = self.agent(agent_id).await
        {
            {
                let mut summary = agent.summary.write().await;
                summary.status = AgentStatus::Failed;
                summary.current_turn = None;
                summary.updated_at = now();
                summary.last_error = Some(err.to_string());
            }
            if let Err(store_err) = self.persist_agent(&agent).await {
                tracing::warn!("failed to persist failed turn state: {store_err}");
            }
            self.publish(ServiceEventKind::Error {
                agent_id: Some(agent_id),
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                message: err.to_string(),
            })
            .await;
            self.publish(ServiceEventKind::TurnCompleted {
                agent_id,
                session_id: Some(session_id),
                turn_id,
                status: TurnStatus::Failed,
            })
            .await;
            self.publish(ServiceEventKind::AgentStatusChanged {
                agent_id,
                status: AgentStatus::Failed,
            })
            .await;
        }
    }

    async fn run_turn_inner(
        self: &Arc<Self>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        mut skill_mentions: Vec<String>,
    ) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        let _turn_guard = agent.turn_lock.lock().await;
        self.ensure_agent_container(&agent, AgentStatus::RunningTurn)
            .await?;
        self.publish(ServiceEventKind::TurnStarted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
        })
        .await;

        skill_mentions.extend(extract_skill_mentions(&message));
        if let Err(err) = self
            .maybe_auto_compact(&agent, agent_id, session_id, turn_id)
            .await
        {
            tracing::warn!("auto context compaction failed before user message: {err}");
        }
        self.record_message(
            &agent,
            agent_id,
            session_id,
            MessageRole::User,
            message.clone(),
        )
        .await?;
        self.record_history_item(
            &agent,
            agent_id,
            session_id,
            ModelInputItem::user_text(message.clone()),
        )
        .await?;
        self.publish(ServiceEventKind::AgentMessage {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            role: MessageRole::User,
            content: message,
        })
        .await;

        let loaded_skills = self.skills.load_explicit(&skill_mentions)?;
        let mut last_assistant_text: Option<String> = None;
        for iteration in 0..MAX_TOOL_ITERATIONS {
            if agent.cancel_requested.load(Ordering::SeqCst) {
                self.finish_turn(
                    &agent,
                    agent_id,
                    session_id,
                    turn_id,
                    TurnStatus::Cancelled,
                    AgentStatus::Cancelled,
                    None,
                )
                .await?;
                return Ok(());
            }

            self.set_status(&agent, AgentStatus::RunningTurn, None)
                .await?;
            let mcp_tools = self.agent_mcp_tools(&agent).await;
            let tools = build_tool_definitions(&mcp_tools);
            let instructions = self
                .build_instructions(&agent, &loaded_skills, &mcp_tools)
                .await?;
            let summary = agent.summary.read().await.clone();
            let model_name = summary.model.clone();
            let provider_id = summary.provider_id.clone();
            let reasoning_effort = summary.reasoning_effort;
            let provider_selection = self
                .store
                .resolve_provider(Some(&provider_id), Some(&model_name))
                .await?;
            if let Err(err) = self
                .maybe_auto_compact(&agent, agent_id, session_id, turn_id)
                .await
            {
                tracing::warn!("auto context compaction failed before model request: {err}");
            }
            let history = self.session_history(&agent, agent_id, session_id).await?;
            let response = self
                .model
                .create_response(
                    &provider_selection.provider,
                    &provider_selection.model,
                    &instructions,
                    &history,
                    &tools,
                    reasoning_effort,
                )
                .await?;

            if let Some(usage) = response.usage {
                {
                    let mut summary = agent.summary.write().await;
                    summary.token_usage.add(&usage);
                    summary.updated_at = now();
                }
                self.persist_agent(&agent).await?;
                self.record_session_context_tokens(
                    &agent,
                    agent_id,
                    session_id,
                    usage.total_tokens,
                )
                .await?;
            }

            let mut tool_calls = Vec::new();
            for item in response.output {
                match item {
                    ModelOutputItem::Message { text } => {
                        if !text.trim().is_empty() {
                            last_assistant_text = Some(text.clone());
                            self.record_message(
                                &agent,
                                agent_id,
                                session_id,
                                MessageRole::Assistant,
                                text.clone(),
                            )
                            .await?;
                            self.record_history_item(
                                &agent,
                                agent_id,
                                session_id,
                                ModelInputItem::assistant_text(text.clone()),
                            )
                            .await?;
                            self.publish(ServiceEventKind::AgentMessage {
                                agent_id,
                                session_id: Some(session_id),
                                turn_id: Some(turn_id),
                                role: MessageRole::Assistant,
                                content: text,
                            })
                            .await;
                        }
                    }
                    ModelOutputItem::FunctionCall {
                        call_id,
                        name,
                        arguments,
                        raw_arguments,
                    } => {
                        let call_id = if call_id.is_empty() {
                            format!("call_{}", Uuid::new_v4())
                        } else {
                            call_id
                        };
                        self.record_history_item(
                            &agent,
                            agent_id,
                            session_id,
                            ModelInputItem::FunctionCall {
                                call_id: call_id.clone(),
                                name: name.clone(),
                                arguments: raw_arguments,
                            },
                        )
                        .await?;
                        tool_calls.push((call_id, name, arguments));
                    }
                    ModelOutputItem::AssistantTurn {
                        content,
                        reasoning_content,
                        tool_calls: output_tool_calls,
                    } => {
                        let assistant_tool_calls = output_tool_calls
                            .into_iter()
                            .map(|tool_call| {
                                let call_id = if tool_call.call_id.is_empty() {
                                    format!("call_{}", Uuid::new_v4())
                                } else {
                                    tool_call.call_id
                                };
                                let name = tool_call.name;
                                let arguments = tool_call.arguments;
                                let raw_arguments = tool_call.raw_arguments;
                                tool_calls.push((call_id.clone(), name.clone(), arguments));
                                ModelToolCall {
                                    call_id,
                                    name,
                                    arguments: raw_arguments,
                                }
                            })
                            .collect::<Vec<_>>();
                        let has_content =
                            content.as_ref().is_some_and(|text| !text.trim().is_empty());
                        let has_reasoning = reasoning_content
                            .as_ref()
                            .is_some_and(|reasoning| !reasoning.trim().is_empty());
                        if has_content || has_reasoning || !assistant_tool_calls.is_empty() {
                            self.record_history_item(
                                &agent,
                                agent_id,
                                session_id,
                                ModelInputItem::AssistantTurn {
                                    content: content.clone().filter(|text| !text.is_empty()),
                                    reasoning_content: reasoning_content
                                        .as_ref()
                                        .filter(|reasoning| !reasoning.trim().is_empty())
                                        .cloned(),
                                    tool_calls: assistant_tool_calls,
                                },
                            )
                            .await?;
                        }
                        if let Some(text) = content.filter(|text| !text.trim().is_empty()) {
                            last_assistant_text = Some(text.clone());
                            self.record_message(
                                &agent,
                                agent_id,
                                session_id,
                                MessageRole::Assistant,
                                text.clone(),
                            )
                            .await?;
                            self.publish(ServiceEventKind::AgentMessage {
                                agent_id,
                                session_id: Some(session_id),
                                turn_id: Some(turn_id),
                                role: MessageRole::Assistant,
                                content: text,
                            })
                            .await;
                        } else if let Some(reasoning) =
                            reasoning_content.as_ref().filter(|r| !r.trim().is_empty())
                        {
                            last_assistant_text = Some(reasoning.clone());
                        }
                    }
                    ModelOutputItem::Other { .. } => {}
                }
            }

            if tool_calls.is_empty() {
                self.finish_turn(
                    &agent,
                    agent_id,
                    session_id,
                    turn_id,
                    TurnStatus::Completed,
                    AgentStatus::Completed,
                    last_assistant_text,
                )
                .await?;
                return Ok(());
            }

            self.set_status(&agent, AgentStatus::WaitingTool, None)
                .await?;
            let mut should_end_turn = false;
            for (call_id, name, arguments) in tool_calls {
                let arguments_preview = trace_preview_value(&arguments, 500);
                let inline_arguments = inline_event_arguments(&arguments);
                self.publish(ServiceEventKind::ToolStarted {
                    agent_id,
                    session_id: Some(session_id),
                    turn_id,
                    call_id: call_id.clone(),
                    tool_name: name.clone(),
                    arguments_preview: Some(arguments_preview),
                    arguments: inline_arguments,
                })
                .await;
                let started_at = Instant::now();
                let output = self
                    .execute_tool(&agent, agent_id, turn_id, &name, arguments)
                    .await;
                let duration_ms = u128_to_u64(started_at.elapsed().as_millis());
                let execution = match output {
                    Ok(execution) => execution,
                    Err(err) => ToolExecution {
                        success: false,
                        output: err.to_string(),
                        ends_turn: false,
                    },
                };
                if execution.ends_turn {
                    should_end_turn = true;
                }
                self.record_history_item(
                    &agent,
                    agent_id,
                    session_id,
                    ModelInputItem::FunctionCallOutput {
                        call_id: call_id.clone(),
                        output: execution.output.clone(),
                    },
                )
                .await?;
                self.publish(ServiceEventKind::ToolCompleted {
                    agent_id,
                    session_id: Some(session_id),
                    turn_id,
                    call_id,
                    tool_name: name,
                    success: execution.success,
                    output_preview: trace_preview_output(&execution.output, 500),
                    duration_ms: Some(duration_ms),
                })
                .await;
            }

            if should_end_turn {
                self.finish_turn(
                    &agent,
                    agent_id,
                    session_id,
                    turn_id,
                    TurnStatus::Completed,
                    AgentStatus::Completed,
                    last_assistant_text,
                )
                .await?;
                return Ok(());
            }

            if iteration + 1 == MAX_TOOL_ITERATIONS {
                return Err(RuntimeError::InvalidInput(format!(
                    "tool iteration limit reached ({MAX_TOOL_ITERATIONS})"
                )));
            }
        }

        Ok(())
    }

    async fn execute_tool(
        self: &Arc<Self>,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        _turn_id: TurnId,
        name: &str,
        arguments: Value,
    ) -> Result<ToolExecution> {
        match route_tool(name) {
            RoutedTool::ContainerExec => {
                let command = required_string(&arguments, "command")?;
                let cwd = optional_string(&arguments, "cwd");
                let timeout = arguments.get("timeout_secs").and_then(Value::as_u64);
                let container_id = self.container_id(agent_id).await?;
                let output = self
                    .docker
                    .exec_shell(&container_id, &command, cwd.as_deref(), timeout)
                    .await?;
                Ok(ToolExecution {
                    success: output.status == 0,
                    output: serde_json::to_string(&json!({
                        "status": output.status,
                        "stdout": output.stdout,
                        "stderr": output.stderr,
                    }))
                    .unwrap_or_else(|_| "{}".to_string()),
                    ends_turn: false,
                })
            }
            RoutedTool::ContainerCpUpload => {
                let path = required_string(&arguments, "path")?;
                let content_base64 = required_string(&arguments, "content_base64")?;
                let bytes = self
                    .upload_file(agent_id, path.clone(), content_base64)
                    .await?;
                Ok(ToolExecution {
                    success: true,
                    output: json!({ "path": path, "bytes": bytes }).to_string(),
                    ends_turn: false,
                })
            }
            RoutedTool::ContainerCpDownload => {
                let path = required_string(&arguments, "path")?;
                let bytes = self.download_file_tar(agent_id, path.clone()).await?;
                Ok(ToolExecution {
                    success: true,
                    output: json!({
                        "path": path,
                        "tar_base64": BASE64.encode(bytes),
                    })
                    .to_string(),
                    ends_turn: false,
                })
            }
            RoutedTool::SpawnAgent => {
                let name = optional_string(&arguments, "name");
                let message = optional_string(&arguments, "message");
                let role = optional_string(&arguments, "role")
                    .as_deref()
                    .map(parse_agent_role)
                    .transpose()?
                    .unwrap_or_default();
                let child_model = self.resolve_role_agent_model(role).await?;
                let parent = self.agent(agent_id).await?;
                let parent_status = parent.summary.read().await.status.clone();
                let parent_summary = parent.summary.read().await.clone();
                let parent_container_id =
                    self.ensure_agent_container(&parent, parent_status).await?;
                let parent_docker_image = parent_summary.docker_image.clone();
                let created = self
                    .create_agent_with_container_source(
                        CreateAgentRequest {
                            name,
                            provider_id: Some(child_model.preference.provider_id),
                            model: Some(child_model.preference.model),
                            reasoning_effort: child_model.preference.reasoning_effort,
                            docker_image: Some(parent_docker_image.clone()),
                            parent_id: Some(agent_id),
                            system_prompt: Some(task_role_system_prompt(role).to_string()),
                        },
                        ContainerSource::CloneFrom {
                            parent_container_id,
                            docker_image: parent_docker_image,
                        },
                        parent_summary.task_id,
                        Some(role),
                    )
                    .await?;
                let turn_id = if let Some(message) = message {
                    let session_id = self.resolve_session_id(created.id, None).await?;
                    let turn_id = self.prepare_turn(created.id).await?;
                    self.spawn_turn(created.id, session_id, turn_id, message, Vec::new());
                    Some(turn_id)
                } else {
                    None
                };
                Ok(ToolExecution {
                    success: true,
                    output: json!({ "agent": created, "turn_id": turn_id }).to_string(),
                    ends_turn: false,
                })
            }
            RoutedTool::SendMessage => {
                let target = parse_agent_id(&required_string(&arguments, "agent_id")?)?;
                let session_id = optional_string(&arguments, "session_id")
                    .as_deref()
                    .map(parse_session_id)
                    .transpose()?;
                let message = required_string(&arguments, "message")?;
                let session_id = self.resolve_session_id(target, session_id).await?;
                let turn_id = self.prepare_turn(target).await?;
                self.spawn_turn(target, session_id, turn_id, message, Vec::new());
                Ok(ToolExecution {
                    success: true,
                    output: json!({ "turn_id": turn_id }).to_string(),
                    ends_turn: false,
                })
            }
            RoutedTool::WaitAgent => {
                let target = parse_agent_id(&required_string(&arguments, "agent_id")?)?;
                let timeout_secs = arguments
                    .get("timeout_secs")
                    .and_then(Value::as_u64)
                    .unwrap_or(300);
                let output = self
                    .wait_agent_output(target, Duration::from_secs(timeout_secs))
                    .await?;
                self.cleanup_finished_explorer_agent(target).await?;
                Ok(ToolExecution {
                    success: true,
                    output: serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string()),
                    ends_turn: false,
                })
            }
            RoutedTool::ListAgents => Ok(ToolExecution {
                success: true,
                output: serde_json::to_string(&self.list_agents().await)
                    .unwrap_or_else(|_| "[]".to_string()),
                ends_turn: false,
            }),
            RoutedTool::CloseAgent => {
                let target = parse_agent_id(&required_string(&arguments, "agent_id")?)?;
                self.delete_agent(target).await?;
                Ok(ToolExecution {
                    success: true,
                    output: json!({ "closed": target }).to_string(),
                    ends_turn: false,
                })
            }
            RoutedTool::SaveTaskPlan => {
                let title = required_string(&arguments, "title")?;
                let markdown = required_string(&arguments, "markdown")?;
                let task = self.save_task_plan(agent_id, title, markdown).await?;
                Ok(ToolExecution {
                    success: true,
                    output: serde_json::to_string(&task).unwrap_or_else(|_| "{}".to_string()),
                    ends_turn: false,
                })
            }
            RoutedTool::SubmitReviewResult => {
                let passed = arguments
                    .get("passed")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| RuntimeError::InvalidInput("missing boolean field `passed`".to_string()))?;
                let findings = required_string(&arguments, "findings")?;
                let summary = required_string(&arguments, "summary")?;
                let review = self
                    .submit_review_result(agent_id, passed, findings, summary)
                    .await?;
                Ok(ToolExecution {
                    success: true,
                    output: serde_json::to_string(&review).unwrap_or_else(|_| "{}".to_string()),
                    ends_turn: false,
                })
            }
            RoutedTool::UpdateTodoList => {
                let items_arg = arguments.get("items")
                    .ok_or_else(|| RuntimeError::InvalidInput("missing field `items`".to_string()))?;
                let items: Vec<TodoItem> = serde_json::from_value(items_arg.clone())
                    .map_err(|e| RuntimeError::InvalidInput(format!("invalid items: {e}")))?;
                self.publish(ServiceEventKind::TodoListUpdated {
                    agent_id,
                    session_id: None,
                    turn_id: _turn_id,
                    items,
                })
                .await;
                Ok(ToolExecution {
                    success: true,
                    output: "Todo list updated".to_string(),
                    ends_turn: false,
                })
            }
            RoutedTool::RequestUserInput => {
                let header = required_string(&arguments, "header")?;
                let questions_arg = arguments.get("questions")
                    .ok_or_else(|| RuntimeError::InvalidInput("missing field `questions`".to_string()))?;
                let raw_questions: Vec<serde_json::Value> = serde_json::from_value(questions_arg.clone())
                    .map_err(|e| RuntimeError::InvalidInput(format!("invalid questions: {e}")))?;
                let mut questions = Vec::with_capacity(raw_questions.len());
                for raw in &raw_questions {
                    let id = raw.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                    let question = raw.get("question").and_then(Value::as_str).unwrap_or_default().to_string();
                    let options_raw = raw.get("options")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let mut options = Vec::with_capacity(options_raw.len());
                    for opt in &options_raw {
                        let label = opt.get("label").and_then(Value::as_str).unwrap_or_default().to_string();
                        let description = opt.get("description").and_then(Value::as_str).map(str::to_string);
                        options.push(UserInputOption { label, description });
                    }
                    questions.push(UserInputQuestion { id, question, options });
                }
                self.publish(ServiceEventKind::UserInputRequested {
                    agent_id,
                    session_id: None,
                    turn_id: _turn_id,
                    header,
                    questions,
                })
                .await;
                Ok(ToolExecution {
                    success: true,
                    output: "Questions sent to user. Wait for their response in the next message.".to_string(),
                    ends_turn: true,
                })
            }
            RoutedTool::SaveArtifact => {
                let path = required_string(&arguments, "path")?;
                let name = optional_string(&arguments, "name");
                let artifact = self.save_artifact(agent_id, path, name).await?;
                Ok(ToolExecution {
                    success: true,
                    output: serde_json::to_string(&artifact)
                        .unwrap_or_else(|_| "{}".to_string()),
                    ends_turn: false,
                })
            }
            RoutedTool::Mcp(model_name) => {
                let manager = agent.mcp.read().await.clone().ok_or_else(|| {
                    RuntimeError::InvalidInput("MCP manager not initialized".to_string())
                })?;
                let output = manager.call_model_tool(&model_name, arguments).await?;
                Ok(ToolExecution {
                    success: true,
                    output: output.to_string(),
                    ends_turn: false,
                })
            }
            RoutedTool::Unknown(name) => Ok(ToolExecution {
                success: false,
                output: format!("unknown tool: {name}"),
                ends_turn: false,
            }),
        }
    }

    async fn save_task_plan(
        self: &Arc<Self>,
        agent_id: AgentId,
        title: String,
        markdown: String,
    ) -> Result<TaskSummary> {
        let agent = self.agent(agent_id).await?;
        let summary = agent.summary.read().await.clone();
        if summary.role != Some(AgentRole::Planner) {
            return Err(RuntimeError::InvalidInput(
                "only planner task agents can save task plans".to_string(),
            ));
        }
        let task_id = summary.task_id.ok_or_else(|| {
            RuntimeError::InvalidInput("agent is not attached to a task".to_string())
        })?;
        let task = self.task(task_id).await?;
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
                self.store.save_plan_history_entry(task_id, &entry).await?;
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
            let mut task_summary = task.summary.write().await;
            task_summary.status = TaskStatus::AwaitingApproval;
            task_summary.plan_status = PlanStatus::Ready;
            task_summary.plan_version = version;
            task_summary.current_agent_id = Some(agent_id);
            task_summary.updated_at = now();
            self.refresh_task_summary_counts(&mut task_summary).await;
            self.store.save_task(&task_summary, &plan).await?;
            self.publish(ServiceEventKind::PlanUpdated {
                task_id,
                plan: plan.clone(),
            })
            .await;
            self.publish(ServiceEventKind::TaskUpdated {
                task: task_summary.clone(),
            })
            .await;
        }
        Ok(self.task_summary(&task).await)
    }

    async fn submit_review_result(
        self: &Arc<Self>,
        agent_id: AgentId,
        passed: bool,
        findings: String,
        summary: String,
    ) -> Result<TaskReview> {
        let agent = self.agent(agent_id).await?;
        let agent_summary = agent.summary.read().await.clone();
        if agent_summary.role != Some(AgentRole::Reviewer) {
            return Err(RuntimeError::InvalidInput(
                "only reviewer task agents can submit review results".to_string(),
            ));
        }
        let task_id = agent_summary.task_id.ok_or_else(|| {
            RuntimeError::InvalidInput("agent is not attached to a task".to_string())
        })?;
        let task = self.task(task_id).await?;
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
            self.store.append_task_review(&review).await?;
            reviews.push(review.clone());
            review
        };
        {
            let plan = task.plan.read().await.clone();
            let mut summary = task.summary.write().await;
            summary.review_rounds = task.reviews.read().await.len() as u64;
            summary.updated_at = now();
            self.refresh_task_summary_counts(&mut summary).await;
            self.store.save_task(&summary, &plan).await?;
            self.publish(ServiceEventKind::TaskUpdated {
                task: summary.clone(),
            })
            .await;
        }
        Ok(review)
    }

    fn spawn_task_workflow(self: &Arc<Self>, task_id: TaskId) {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = runtime.clone().run_task_workflow(task_id).await
                && let Ok(task) = runtime.task(task_id).await
            {
                let _ = runtime
                    .set_task_status(&task, TaskStatus::Failed, None, Some(err.to_string()))
                    .await;
            }
        });
    }

    async fn run_task_workflow(self: Arc<Self>, task_id: TaskId) -> Result<()> {
        let task = self.task(task_id).await?;
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
        let executor = self
            .spawn_task_role_agent(
                planner_agent_id,
                AgentRole::Executor,
                Some("Task Executor".to_string()),
            )
            .await?;
        self.set_task_current_agent(&task, executor.id, TaskStatus::Executing, None)
            .await?;
        self.start_agent_turn(
            executor.id,
            format!(
                "Implement the approved task plan below. Keep changes scoped, run verification, and report touched files and test results.\n\n{}",
                plan_markdown
            ),
        )
        .await?;
        let mut executor_summary = self
            .wait_agent(executor.id, Duration::from_secs(3600))
            .await?;
        for round in 1..=REVIEW_ROUND_LIMIT {
            if matches!(executor_summary.status, AgentStatus::Failed | AgentStatus::Cancelled) {
                return Err(RuntimeError::InvalidInput(format!(
                    "executor ended with status {:?}",
                    executor_summary.status
                )));
            }
            let reviewer = self
                .spawn_task_role_agent(
                    executor.id,
                    AgentRole::Reviewer,
                    Some(format!("Task Reviewer {round}")),
                )
                .await?;
            self.set_task_current_agent(&task, reviewer.id, TaskStatus::Reviewing, None)
                .await?;
            self.start_agent_turn(
                reviewer.id,
                format!(
                    "Review the executor's changes for the approved task plan. Use submit_review_result with passed=true only when there are no blocking issues. Include concrete findings and a concise summary.\n\nApproved plan:\n{}",
                    plan_markdown
                ),
            )
            .await?;
            let reviewer_summary = self
                .wait_agent(reviewer.id, Duration::from_secs(3600))
                .await?;
            if matches!(reviewer_summary.status, AgentStatus::Failed | AgentStatus::Cancelled) {
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
                self.set_task_status(&task, TaskStatus::Completed, Some(report), None)
                    .await?;
                return Ok(());
            }
            if round == REVIEW_ROUND_LIMIT {
                self.set_task_status(
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
            self.set_task_current_agent(&task, executor.id, TaskStatus::Executing, None)
                .await?;
            self.start_agent_turn(
                executor.id,
                format!(
                    "The reviewer found issues. Fix them, rerun verification, and report the changes.\n\nReview findings:\n{}\n\nReview summary:\n{}",
                    review.findings, review.summary
                ),
            )
            .await?;
            executor_summary = self
                .wait_agent(executor.id, Duration::from_secs(3600))
                .await?;
        }
        Ok(())
    }

    async fn spawn_task_role_agent(
        self: &Arc<Self>,
        parent_agent_id: AgentId,
        role: AgentRole,
        name: Option<String>,
    ) -> Result<AgentSummary> {
        let parent = self.agent(parent_agent_id).await?;
        let parent_summary = parent.summary.read().await.clone();
        let task_id = parent_summary.task_id.ok_or_else(|| {
            RuntimeError::InvalidInput("parent agent is not attached to a task".to_string())
        })?;
        let parent_container_id = self
            .ensure_agent_container(&parent, parent_summary.status.clone())
            .await?;
        let model = self.resolve_role_agent_model(role).await?;
        self.create_agent_with_container_source(
            CreateAgentRequest {
                name,
                provider_id: Some(model.preference.provider_id),
                model: Some(model.preference.model),
                reasoning_effort: model.preference.reasoning_effort,
                docker_image: Some(parent_summary.docker_image.clone()),
                parent_id: Some(parent_agent_id),
                system_prompt: Some(task_role_system_prompt(role).to_string()),
            },
            ContainerSource::CloneFrom {
                parent_container_id,
                docker_image: parent_summary.docker_image,
            },
            Some(task_id),
            Some(role),
        )
        .await
    }

    async fn start_agent_turn(
        self: &Arc<Self>,
        agent_id: AgentId,
        message: String,
    ) -> Result<TurnId> {
        let session_id = self.resolve_session_id(agent_id, None).await?;
        let turn_id = self.prepare_turn(agent_id).await?;
        self.spawn_turn(agent_id, session_id, turn_id, message, Vec::new());
        Ok(turn_id)
    }

    #[cfg(test)]
    async fn execute_tool_for_test(
        self: &Arc<Self>,
        agent_id: AgentId,
        name: &str,
        arguments: Value,
    ) -> Result<ToolExecution> {
        let agent = self.agent(agent_id).await?;
        self.execute_tool(&agent, agent_id, Uuid::new_v4(), name, arguments)
            .await
    }

    async fn wait_agent(&self, agent_id: AgentId, timeout: Duration) -> Result<AgentSummary> {
        let deadline = Instant::now() + timeout;
        loop {
            let agent = self.agent(agent_id).await?;
            let summary = agent.summary.read().await.clone();
            if summary.current_turn.is_none()
                || matches!(
                    summary.status,
                    AgentStatus::Completed
                        | AgentStatus::Failed
                        | AgentStatus::Cancelled
                        | AgentStatus::Deleted
                        | AgentStatus::Idle
                )
            {
                return Ok(summary);
            }
            if Instant::now() >= deadline {
                return Ok(summary);
            }
            sleep(Duration::from_millis(250)).await;
        }
    }

    async fn wait_agent_output(&self, agent_id: AgentId, timeout: Duration) -> Result<Value> {
        let agent = self.wait_agent(agent_id, timeout).await?;
        let (session_id, recent_messages) = self.agent_recent_messages(agent_id, 12).await?;
        let tracked_response = {
            let agent_rec = self.agent(agent_id).await?;
            let sessions = agent_rec.sessions.lock().await;
            sessions
                .iter()
                .filter_map(|s| s.last_turn_response.as_ref())
                .last()
                .cloned()
        };
        let final_response = tracked_response.or_else(|| {
            recent_messages
                .iter()
                .rev()
                .find(|message| message.role == MessageRole::Assistant)
                .map(|message| message.content.clone())
        });
        Ok(json!({
            "agent": agent,
            "session_id": session_id,
            "final_response": final_response,
            "recent_messages": recent_messages,
        }))
    }

    async fn cleanup_finished_explorer_agent(&self, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        let summary = agent.summary.read().await.clone();
        if summary.role != Some(AgentRole::Explorer) {
            return Ok(());
        }
        if summary.current_turn.is_some()
            || matches!(
                summary.status,
                AgentStatus::Created
                    | AgentStatus::StartingContainer
                    | AgentStatus::RunningTurn
                    | AgentStatus::WaitingTool
                    | AgentStatus::DeletingContainer
            )
        {
            return Ok(());
        }
        drop(agent);
        self.delete_agent(agent_id).await
    }

    async fn agent_recent_messages(
        &self,
        agent_id: AgentId,
        limit: usize,
    ) -> Result<(Option<SessionId>, Vec<AgentMessage>)> {
        let agent = self.agent(agent_id).await?;
        let sessions = agent.sessions.lock().await;
        let Some(session) = selected_session(&sessions, None) else {
            return Ok((None, Vec::new()));
        };
        let len = session.messages.len();
        let start = len.saturating_sub(limit);
        Ok((
            Some(session.summary.id),
            session.messages[start..].to_vec(),
        ))
    }

    async fn build_instructions(
        &self,
        agent: &AgentRecord,
        loaded_skills: &[mai_skills::LoadedSkill],
        mcp_tools: &[mai_mcp::McpTool],
    ) -> Result<String> {
        let mut instructions = String::from(BASE_INSTRUCTIONS);
        if let Some(system_prompt) = &agent.system_prompt {
            instructions.push_str("\n\n## Agent System Prompt\n");
            instructions.push_str(system_prompt);
        }
        instructions.push_str("\n\n## Available Skills\n");
        instructions.push_str(&self.skills.render_available()?);
        if !loaded_skills.is_empty() {
            instructions.push_str("\n\n## Loaded Skill Instructions\n");
            for skill in loaded_skills {
                instructions.push_str(&format!(
                    "\n### ${}\nPath: {}\n{}\n",
                    skill.summary.name,
                    skill.summary.path.display(),
                    skill.contents
                ));
            }
        }
        instructions.push_str("\n\n## MCP Tools\n");
        if mcp_tools.is_empty() {
            instructions.push_str("No MCP tools are currently available.");
        } else {
            for tool in mcp_tools {
                instructions.push_str(&format!(
                    "\n- {} maps to MCP `{}` on server `{}`",
                    tool.model_name, tool.name, tool.server
                ));
            }
        }
        Ok(instructions)
    }

    async fn finish_turn(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        turn_status: TurnStatus,
        agent_status: AgentStatus,
        final_text: Option<String>,
    ) -> Result<()> {
        {
            let mut summary = agent.summary.write().await;
            summary.status = agent_status.clone();
            summary.current_turn = None;
            summary.updated_at = now();
        }
        {
            let mut sessions = agent.sessions.lock().await;
            if let Some(session) = sessions.iter_mut().find(|s| s.summary.id == session_id) {
                session.last_turn_response = final_text;
            }
        }
        self.persist_agent(agent).await?;
        self.publish(ServiceEventKind::TurnCompleted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
            status: turn_status,
        })
        .await;
        self.publish(ServiceEventKind::AgentStatusChanged {
            agent_id,
            status: agent_status,
        })
        .await;
        Ok(())
    }

    async fn set_status(
        &self,
        agent: &Arc<AgentRecord>,
        status: AgentStatus,
        error: Option<String>,
    ) -> Result<()> {
        let agent_id = {
            let mut summary = agent.summary.write().await;
            summary.status = status.clone();
            summary.updated_at = now();
            if let Some(error) = error {
                summary.last_error = Some(error);
            }
            summary.id
        };
        self.persist_agent(agent).await?;
        self.publish(ServiceEventKind::AgentStatusChanged { agent_id, status })
            .await;
        Ok(())
    }

    async fn record_message(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
        role: MessageRole,
        content: String,
    ) -> Result<()> {
        let message = AgentMessage {
            role,
            content,
            created_at: now(),
        };
        let (position, session_summary) = {
            let mut sessions = agent.sessions.lock().await;
            let session = sessions
                .iter_mut()
                .find(|session| session.summary.id == session_id)
                .ok_or(RuntimeError::SessionNotFound {
                    agent_id,
                    session_id,
                })?;
            let position = session.messages.len();
            session.messages.push(message.clone());
            session.summary.message_count = session.messages.len();
            session.summary.updated_at = message.created_at;
            (position, session.summary.clone())
        };
        self.store
            .save_agent_session(agent_id, &session_summary)
            .await?;
        self.store
            .append_agent_message(agent_id, session_id, position, &message)
            .await?;
        Ok(())
    }

    async fn record_history_item(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
        item: ModelInputItem,
    ) -> Result<()> {
        let position = {
            let mut sessions = agent.sessions.lock().await;
            let session = sessions
                .iter_mut()
                .find(|session| session.summary.id == session_id)
                .ok_or(RuntimeError::SessionNotFound {
                    agent_id,
                    session_id,
                })?;
            let position = session.history.len();
            session.history.push(item.clone());
            position
        };
        self.store
            .append_agent_history_item(agent_id, session_id, position, &item)
            .await?;
        Ok(())
    }

    async fn replace_session_history(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
        history: Vec<ModelInputItem>,
    ) -> Result<()> {
        self.store
            .replace_agent_history(agent_id, session_id, &history)
            .await?;
        {
            let mut sessions = agent.sessions.lock().await;
            let session = sessions
                .iter_mut()
                .find(|session| session.summary.id == session_id)
                .ok_or(RuntimeError::SessionNotFound {
                    agent_id,
                    session_id,
                })?;
            session.history = history.clone();
            session.last_context_tokens = None;
        }
        Ok(())
    }

    async fn record_session_context_tokens(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
        tokens: u64,
    ) -> Result<()> {
        {
            let mut sessions = agent.sessions.lock().await;
            let session = sessions
                .iter_mut()
                .find(|session| session.summary.id == session_id)
                .ok_or(RuntimeError::SessionNotFound {
                    agent_id,
                    session_id,
                })?;
            session.last_context_tokens = Some(tokens);
        }
        self.store
            .save_session_context_tokens(agent_id, session_id, tokens)
            .await?;
        Ok(())
    }

    async fn maybe_auto_compact(
        self: &Arc<Self>,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<()> {
        let last_context_tokens = self
            .session_context_tokens(agent, agent_id, session_id)
            .await?;
        let Some(tokens_before) = last_context_tokens else {
            return Ok(());
        };
        let summary = agent.summary.read().await.clone();
        let provider_selection = self
            .store
            .resolve_provider(Some(&summary.provider_id), Some(&summary.model))
            .await?;
        if !should_auto_compact(tokens_before, provider_selection.model.context_tokens) {
            return Ok(());
        }

        let history = self.session_history(agent, agent_id, session_id).await?;
        if history.is_empty() {
            self.record_session_context_tokens(agent, agent_id, session_id, 0)
                .await?;
            return Ok(());
        }
        let mut compact_input = history.clone();
        compact_input.push(ModelInputItem::user_text(COMPACT_PROMPT));
        let instructions = self.build_instructions(agent, &[], &[]).await?;
        let response = self
            .model
            .create_response(
                &provider_selection.provider,
                &provider_selection.model,
                &instructions,
                &compact_input,
                &[],
                summary.reasoning_effort,
            )
            .await?;

        if let Some(usage) = response.usage {
            {
                let mut summary = agent.summary.write().await;
                summary.token_usage.add(&usage);
                summary.updated_at = now();
            }
            self.persist_agent(agent).await?;
        }

        let summary_text = compact_summary_from_output(&response.output).ok_or_else(|| {
            RuntimeError::InvalidInput("compact response did not include a summary".to_string())
        })?;
        let replacement = build_compacted_history(&history, &summary_text);
        self.replace_session_history(agent, agent_id, session_id, replacement)
            .await?;
        self.publish(ServiceEventKind::ContextCompacted {
            agent_id,
            session_id,
            turn_id,
            tokens_before,
            summary_preview: preview(&summary_text, COMPACT_SUMMARY_PREVIEW_CHARS),
        })
        .await;
        Ok(())
    }

    async fn session_context_tokens(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<Option<u64>> {
        let sessions = agent.sessions.lock().await;
        sessions
            .iter()
            .find(|session| session.summary.id == session_id)
            .map(|session| session.last_context_tokens)
            .ok_or(RuntimeError::SessionNotFound {
                agent_id,
                session_id,
            })
    }

    async fn persist_agent(&self, agent: &AgentRecord) -> Result<()> {
        let summary = agent.summary.read().await.clone();
        self.store
            .save_agent(&summary, agent.system_prompt.as_deref())
            .await?;
        Ok(())
    }

    async fn task(&self, task_id: TaskId) -> Result<Arc<TaskRecord>> {
        self.tasks
            .read()
            .await
            .get(&task_id)
            .cloned()
            .ok_or(RuntimeError::TaskNotFound(task_id))
    }

    async fn task_summary(&self, task: &Arc<TaskRecord>) -> TaskSummary {
        let mut summary = task.summary.read().await.clone();
        self.refresh_task_summary_counts(&mut summary).await;
        summary
    }

    async fn refresh_task_summary_counts(&self, summary: &mut TaskSummary) {
        summary.agent_count = self.task_agents(summary.id).await.len();
        let task = {
            let tasks = self.tasks.read().await;
            tasks.get(&summary.id).cloned()
        };
        if let Some(task) = task {
            summary.review_rounds = task.reviews.read().await.len() as u64;
        }
    }

    async fn task_agents(&self, task_id: TaskId) -> Vec<AgentSummary> {
        let agents = self.agents.read().await;
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

    async fn set_task_current_agent(
        &self,
        task: &Arc<TaskRecord>,
        agent_id: AgentId,
        status: TaskStatus,
        error: Option<String>,
    ) -> Result<()> {
        let plan = task.plan.read().await.clone();
        let mut summary = task.summary.write().await;
        summary.current_agent_id = Some(agent_id);
        summary.status = status;
        summary.updated_at = now();
        if let Some(error) = error {
            summary.last_error = Some(error);
        }
        summary.plan_status = plan.status.clone();
        summary.plan_version = plan.version;
        self.refresh_task_summary_counts(&mut summary).await;
        self.store.save_task(&summary, &plan).await?;
        self.publish(ServiceEventKind::TaskUpdated {
            task: summary.clone(),
        })
        .await;
        Ok(())
    }

    async fn set_task_status(
        &self,
        task: &Arc<TaskRecord>,
        status: TaskStatus,
        final_report: Option<String>,
        error: Option<String>,
    ) -> Result<()> {
        let plan = task.plan.read().await.clone();
        let mut summary = task.summary.write().await;
        summary.status = status;
        summary.updated_at = now();
        if final_report.is_some() {
            summary.final_report = final_report;
        }
        if error.is_some() {
            summary.last_error = error;
        }
        summary.plan_status = plan.status.clone();
        summary.plan_version = plan.version;
        self.refresh_task_summary_counts(&mut summary).await;
        self.store.save_task(&summary, &plan).await?;
        self.publish(ServiceEventKind::TaskUpdated {
            task: summary.clone(),
        })
        .await;
        Ok(())
    }

    async fn resolve_session_id(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> Result<SessionId> {
        let agent = self.agent(agent_id).await?;
        let sessions = agent.sessions.lock().await;
        selected_session(&sessions, session_id)
            .map(|session| session.summary.id)
            .ok_or_else(|| RuntimeError::SessionNotFound {
                agent_id,
                session_id: session_id.unwrap_or_default(),
            })
    }

    async fn resolve_role_agent_model(&self, role: AgentRole) -> Result<ResolvedAgentModel> {
        let config = self.store.load_agent_config().await?;
        self.resolve_agent_model_preference(role, role_preference(&config, role))
            .await
    }

    async fn resolve_effective_agent_model(
        &self,
        role: AgentRole,
        preference: Option<&AgentModelPreference>,
        validation_errors: &mut Vec<String>,
    ) -> Option<ResolvedAgentModelPreference> {
        match self.resolve_agent_model_preference(role, preference).await {
            Ok(resolved) => Some(resolved.effective),
            Err(err) => {
                validation_errors.push(err.to_string());
                None
            }
        }
    }

    async fn resolve_agent_model_preference(
        &self,
        role: AgentRole,
        preference: Option<&AgentModelPreference>,
    ) -> Result<ResolvedAgentModel> {
        if let Some(preference) = preference
            && (preference.provider_id.trim().is_empty() || preference.model.trim().is_empty())
        {
            return Err(RuntimeError::InvalidInput(format!(
                "{} provider and model are required",
                agent_role_label(role)
            )));
        }
        let selection = self
            .store
            .resolve_provider(
                preference.map(|item| item.provider_id.as_str()),
                preference.map(|item| item.model.as_str()),
            )
            .await?;
        let reasoning_effort = normalize_reasoning_effort(
            &selection.model,
            preference.and_then(|item| item.reasoning_effort.as_deref()),
            true,
        )?;
        Ok(resolved_agent_model(selection, reasoning_effort))
    }

    async fn session_history(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<Vec<ModelInputItem>> {
        let sessions = agent.sessions.lock().await;
        let mut history = sessions
            .iter()
            .find(|session| session.summary.id == session_id)
            .map(|session| session.history.clone())
            .ok_or(RuntimeError::SessionNotFound {
                agent_id,
                session_id,
            })?;
        repair_incomplete_tool_history(&mut history);
        Ok(history)
    }

    async fn ensure_agent_container(
        &self,
        agent: &Arc<AgentRecord>,
        ready_status: AgentStatus,
    ) -> Result<String> {
        self.ensure_agent_container_with_source(agent, ready_status, &ContainerSource::FreshImage)
            .await
    }

    async fn ensure_agent_container_with_source(
        &self,
        agent: &Arc<AgentRecord>,
        ready_status: AgentStatus,
        container_source: &ContainerSource,
    ) -> Result<String> {
        if let Some(container_id) = agent
            .container
            .read()
            .await
            .as_ref()
            .map(|container| container.id.clone())
        {
            return Ok(container_id);
        }

        let (agent_id, preferred_container_id, docker_image) = {
            let summary = agent.summary.read().await;
            (
                summary.id,
                summary.container_id.clone(),
                summary.docker_image.clone(),
            )
        };
        let mut container_guard = agent.container.write().await;
        if let Some(container_id) = container_guard
            .as_ref()
            .map(|container| container.id.clone())
        {
            return Ok(container_id);
        }

        self.set_status(agent, AgentStatus::StartingContainer, None)
            .await?;
        let container_result = match container_source {
            ContainerSource::FreshImage => {
                self.docker
                    .ensure_agent_container_from_image(
                        &agent_id.to_string(),
                        preferred_container_id.as_deref(),
                        &docker_image,
                    )
                    .await
            }
            ContainerSource::CloneFrom {
                parent_container_id,
                docker_image,
            } => {
                if preferred_container_id.is_some() {
                    self.docker
                        .ensure_agent_container_from_image(
                            &agent_id.to_string(),
                            preferred_container_id.as_deref(),
                            docker_image,
                        )
                        .await
                } else {
                    self.docker
                        .create_agent_container_from_parent(
                            &agent_id.to_string(),
                            parent_container_id,
                        )
                        .await
                }
            }
        };
        let container = match container_result {
            Ok(container) => container,
            Err(err) => {
                let message = err.to_string();
                drop(container_guard);
                if let Err(store_err) = self
                    .set_status(agent, AgentStatus::Failed, Some(message))
                    .await
                {
                    tracing::warn!("failed to persist container startup failure: {store_err}");
                }
                return Err(err.into());
            }
        };

        let container_id = container.id.clone();
        {
            let mut summary = agent.summary.write().await;
            summary.container_id = Some(container_id.clone());
            summary.updated_at = now();
        }
        self.persist_agent(agent).await?;
        *container_guard = Some(container.clone());
        drop(container_guard);

        let mcp = McpAgentManager::start(
            self.docker.clone(),
            container.id,
            self.store.list_mcp_servers().await?,
        )
        .await;
        *agent.mcp.write().await = Some(Arc::new(mcp));
        self.set_status(agent, ready_status, None).await?;
        Ok(container_id)
    }

    async fn agent(&self, agent_id: AgentId) -> Result<Arc<AgentRecord>> {
        self.agents
            .read()
            .await
            .get(&agent_id)
            .cloned()
            .ok_or(RuntimeError::AgentNotFound(agent_id))
    }

    async fn container_id(&self, agent_id: AgentId) -> Result<String> {
        let agent = self.agent(agent_id).await?;
        if let Some(container_id) = agent
            .container
            .read()
            .await
            .as_ref()
            .map(|container| container.id.clone())
        {
            return Ok(container_id);
        }
        let ready_status = agent.summary.read().await.status.clone();
        self.ensure_agent_container(&agent, ready_status).await
    }

    fn resolve_docker_image(&self, requested: Option<&str>) -> String {
        requested
            .map(str::trim)
            .filter(|image| !image.is_empty())
            .unwrap_or_else(|| self.docker.image())
            .to_string()
    }

    async fn agent_mcp_tools(&self, agent: &AgentRecord) -> Vec<mai_mcp::McpTool> {
        let Some(manager) = agent.mcp.read().await.clone() else {
            return Vec::new();
        };
        manager.tools().await
    }

    async fn tool_event_metadata(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        call_id: &str,
    ) -> (Option<bool>, Option<u64>) {
        let events = self.recent_events.lock().await;
        events
            .iter()
            .rev()
            .find_map(|event| match &event.kind {
                ServiceEventKind::ToolCompleted {
                    agent_id: event_agent_id,
                    session_id: event_session_id,
                    call_id: event_call_id,
                    success,
                    duration_ms,
                    ..
                } if *event_agent_id == agent_id
                    && event_session_id == &Some(session_id)
                    && event_call_id == call_id =>
                {
                    Some((Some(*success), *duration_ms))
                }
                _ => None,
            })
            .unwrap_or((None, None))
    }

    async fn publish(&self, kind: ServiceEventKind) {
        let event = ServiceEvent {
            sequence: self.sequence.fetch_add(1, Ordering::SeqCst),
            timestamp: now(),
            kind,
        };
        if let Err(err) = self.store.append_service_event(&event).await {
            tracing::warn!("failed to persist service event: {err}");
        }
        {
            let mut recent = self.recent_events.lock().await;
            if recent.len() >= RECENT_EVENT_LIMIT {
                recent.pop_front();
            }
            recent.push_back(event.clone());
        }
        let _ = self.event_tx.send(event);
    }
}

fn required_string(arguments: &Value, field: &str) -> Result<String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| RuntimeError::InvalidInput(format!("missing string field `{field}`")))
}

fn optional_string(arguments: &Value, field: &str) -> Option<String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn parse_agent_role(value: &str) -> Result<AgentRole> {
    match value.trim().to_lowercase().as_str() {
        "" | "executor" => Ok(AgentRole::Executor),
        "planner" => Ok(AgentRole::Planner),
        "explorer" => Ok(AgentRole::Explorer),
        "reviewer" => Ok(AgentRole::Reviewer),
        _ => Err(RuntimeError::InvalidInput(format!(
            "invalid agent role `{value}`; expected planner, explorer, executor, or reviewer"
        ))),
    }
}

fn parse_agent_id(value: &str) -> Result<AgentId> {
    Uuid::parse_str(value)
        .map_err(|err| RuntimeError::InvalidInput(format!("invalid agent_id `{value}`: {err}")))
}

fn parse_session_id(value: &str) -> Result<SessionId> {
    Uuid::parse_str(value)
        .map_err(|err| RuntimeError::InvalidInput(format!("invalid session_id `{value}`: {err}")))
}

fn resolved_agent_model(
    selection: ProviderSelection,
    reasoning_effort: Option<String>,
) -> ResolvedAgentModel {
    let effective = resolved_agent_model_preference(selection.clone(), reasoning_effort.clone());
    ResolvedAgentModel {
        preference: AgentModelPreference {
            provider_id: selection.provider.id.clone(),
            model: selection.model.id.clone(),
            reasoning_effort,
        },
        effective,
    }
}

fn resolved_agent_model_preference(
    selection: ProviderSelection,
    reasoning_effort: Option<String>,
) -> ResolvedAgentModelPreference {
    ResolvedAgentModelPreference {
        provider_id: selection.provider.id,
        provider_name: selection.provider.name,
        provider_kind: selection.provider.kind,
        model: selection.model.id,
        model_name: selection.model.name,
        reasoning_effort,
        context_tokens: selection.model.context_tokens,
        output_tokens: selection.model.output_tokens,
    }
}

fn role_preference(config: &AgentConfigRequest, role: AgentRole) -> Option<&AgentModelPreference> {
    match role {
        AgentRole::Planner => config.planner.as_ref(),
        AgentRole::Explorer => config.explorer.as_ref(),
        AgentRole::Executor => config.executor.as_ref(),
        AgentRole::Reviewer => config.reviewer.as_ref(),
    }
}

fn agent_role_label(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Planner => "planner",
        AgentRole::Explorer => "explorer",
        AgentRole::Executor => "executor",
        AgentRole::Reviewer => "reviewer",
    }
}

const PLANNER_SYSTEM_PROMPT: &str = r#"You are the Planner for a Mai task. Your job is to create a decision-complete implementation plan through a structured 3-phase process. A decision-complete plan can be handed to the Executor agent and implemented without any additional design decisions.

## 3-Phase Planning Process

### Phase 1 — Explore (discover facts, eliminate unknowns)
- Use `spawn_agent` with role `explorer` to investigate code, docs, and relevant context.
- Run read-only commands to understand the codebase structure, existing patterns, and constraints.
- Do NOT ask the user questions that can be answered by exploring the code.
- Only ask clarifying questions about the prompt if there are obvious ambiguities.

### Phase 2 — Intent Chat (clarify what they want)
- Use `request_user_input` to ask structured questions about: goal + success criteria, scope, constraints, and key preferences/tradeoffs.
- Each question must materially change the plan, confirm an assumption, or choose between meaningful tradeoffs.
- Offer 2-4 clear options with a recommended default.
- Bias toward asking over guessing when high-impact ambiguity remains.

### Phase 3 — Implementation Spec (produce the plan)
- Create a complete implementation specification covering: approach, interfaces/data flow, edge cases, testing strategy, and assumptions.
- The plan must be decision-complete — the Executor should not need to make any design decisions.

## Rules

- **No code modification**: Only explore and plan. Never edit files or make changes.
- **Use `save_task_plan`** to save or update the plan with a clear title and complete Markdown content.
- **Use `update_todo_list`** to show your planning progress to the user.
- **Use `request_user_input`** for structured questions during planning.
- When the user requests revision of the plan, address their feedback fully and save an updated plan.

## Plan Format

The plan should include:
- A clear title
- A brief summary
- Key changes grouped by subsystem or behavior
- Important API/interface changes
- Test cases and scenarios
- Explicit assumptions and defaults chosen

Keep the plan concise and actionable. Prefer behavior-level descriptions over file-by-file inventories. Mention specific files only when needed to disambiguate a non-obvious change."#;

fn task_role_system_prompt(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Planner => PLANNER_SYSTEM_PROMPT,
        AgentRole::Explorer => {
            "You are an Explorer subagent for a task. Investigate code, docs, and relevant context using read-only exploration unless explicitly told otherwise. Return concise findings with concrete files, commands, or sources that help the planner decide."
        }
        AgentRole::Executor => {
            "You are the Executor for an approved task plan. Implement the requested changes in your container, keep scope tight, run verification, and report changed files plus test results. If reviewer feedback arrives, fix the issues and rerun relevant checks."
        }
        AgentRole::Reviewer => {
            "You are the Reviewer for a task workflow. Review executor changes for bugs, regressions, missing tests, and unclear behavior. You must call submit_review_result with passed, findings, and summary before finishing. Set passed=true only when there are no blocking issues."
        }
    }
}

fn descendant_delete_order_from_summaries(
    root_id: AgentId,
    summaries: &[AgentSummary],
) -> Vec<AgentId> {
    let mut children: HashMap<AgentId, Vec<&AgentSummary>> = HashMap::new();
    for summary in summaries {
        if let Some(parent_id) = summary.parent_id {
            children.entry(parent_id).or_default().push(summary);
        }
    }
    for values in children.values_mut() {
        values.sort_by_key(|summary| summary.created_at);
    }

    let mut order = Vec::new();
    push_delete_order(root_id, &children, &mut order);
    order
}

fn push_delete_order(
    agent_id: AgentId,
    children: &HashMap<AgentId, Vec<&AgentSummary>>,
    order: &mut Vec<AgentId>,
) {
    if let Some(child_summaries) = children.get(&agent_id) {
        for child in child_summaries {
            push_delete_order(child.id, children, order);
        }
    }
    order.push(agent_id);
}

fn normalize_reasoning_effort(
    model: &ModelConfig,
    effort: Option<&str>,
    default_when_missing: bool,
) -> Result<Option<String>> {
    let Some(reasoning) = &model.reasoning else {
        return Ok(None);
    };
    match effort {
        Some(value) if value.trim().is_empty() || value == "none" => Ok(None),
        Some(value) if reasoning.variants.iter().any(|variant| variant.id == value) => {
            Ok(Some(value.to_string()))
        }
        Some(effort) => Err(RuntimeError::InvalidInput(format!(
            "reasoning effort `{}` is not supported by model `{}`",
            effort, model.id
        ))),
        None if default_when_missing => Ok(default_reasoning_effort(model)),
        None => Ok(None),
    }
}

fn default_reasoning_effort(model: &ModelConfig) -> Option<String> {
    let reasoning = model.reasoning.as_ref()?;
    reasoning
        .default_variant
        .as_ref()
        .filter(|variant| {
            reasoning
                .variants
                .iter()
                .any(|item| item.id == variant.as_str())
        })
        .cloned()
        .or_else(|| reasoning.variants.first().map(|variant| variant.id.clone()))
}

fn parse_tool_arguments(raw_arguments: &str) -> Value {
    serde_json::from_str(raw_arguments).unwrap_or_else(|_| json!({ "raw": raw_arguments }))
}

fn trace_preview_value(value: &Value, max: usize) -> String {
    let redacted = redacted_preview_value(value);
    let serialized =
        serde_json::to_string_pretty(&redacted).unwrap_or_else(|_| redacted.to_string());
    preview(&serialized, max)
}

fn trace_preview_output(output: &str, max: usize) -> String {
    serde_json::from_str::<Value>(output)
        .map(|value| trace_preview_value(&value, max))
        .unwrap_or_else(|_| preview(&redact_preview_string(output), max))
}

fn inline_event_arguments(value: &Value) -> Option<Value> {
    let redacted = redacted_preview_value(value);
    let serialized = serde_json::to_string(&redacted).ok()?;
    (serialized.len() <= 2_000).then_some(redacted)
}

fn redacted_preview_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                if is_sensitive_key(key) {
                    out.insert(key.clone(), Value::String("<redacted>".to_string()));
                } else {
                    out.insert(key.clone(), redacted_preview_value(value));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(20)
                .map(redacted_preview_value)
                .chain(
                    (items.len() > 20)
                        .then(|| Value::String(format!("<{} more items>", items.len() - 20))),
                )
                .collect(),
        ),
        Value::String(value) => Value::String(redact_preview_string(value)),
        _ => value.clone(),
    }
}

fn redact_preview_string(value: &str) -> String {
    if value.len() > 240 && looks_like_base64(value) {
        return format!("<base64 elided: {} chars>", value.len());
    }
    if value.len() > 800 {
        return format!("{}...", value.chars().take(800).collect::<String>());
    }
    value.to_string()
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("token")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("authorization")
        || key.contains("api_key")
        || key.ends_with("_key")
        || key.contains("base64")
}

fn looks_like_base64(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.len() > 240
        && trimmed.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'\n' | b'\r')
        })
}

fn u128_to_u64(value: u128) -> u64 {
    value.min(u64::MAX as u128) as u64
}

fn recovered_summary(mut summary: AgentSummary) -> (AgentSummary, bool) {
    let mut changed = false;
    if summary.current_turn.take().is_some() {
        changed = true;
    }
    if matches!(
        summary.status,
        AgentStatus::Created
            | AgentStatus::StartingContainer
            | AgentStatus::RunningTurn
            | AgentStatus::WaitingTool
            | AgentStatus::DeletingContainer
    ) {
        summary.status = AgentStatus::Failed;
        summary.last_error = Some("interrupted by server restart".to_string());
        summary.updated_at = now();
        changed = true;
    }
    (summary, changed)
}

fn default_session_record() -> AgentSessionRecord {
    session_record_with_title("Chat 1")
}

fn session_record_with_title(title: &str) -> AgentSessionRecord {
    let now = now();
    AgentSessionRecord {
        summary: AgentSessionSummary {
            id: Uuid::new_v4(),
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            message_count: 0,
        },
        messages: Vec::new(),
        history: Vec::new(),
        last_context_tokens: None,
        last_turn_response: None,
    }
}

fn selected_session(
    sessions: &[AgentSessionRecord],
    session_id: Option<SessionId>,
) -> Option<&AgentSessionRecord> {
    if let Some(session_id) = session_id {
        return sessions
            .iter()
            .find(|session| session.summary.id == session_id);
    }
    sessions
        .iter()
        .max_by(|left, right| {
            left.summary
                .updated_at
                .cmp(&right.summary.updated_at)
                .then_with(|| left.summary.created_at.cmp(&right.summary.created_at))
        })
        .or_else(|| sessions.first())
}

fn short_id(id: AgentId) -> String {
    id.to_string().chars().take(8).collect()
}

fn extract_skill_mentions(text: &str) -> Vec<String> {
    text.split(|ch: char| ch.is_whitespace() || ch == ',' || ch == ')' || ch == '(')
        .filter_map(|part| part.strip_prefix('$'))
        .map(|part| {
            part.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_')
                .to_string()
        })
        .filter(|part| !part.is_empty())
        .collect()
}

fn should_auto_compact(last_context_tokens: u64, context_tokens: u64) -> bool {
    if last_context_tokens == 0 || context_tokens == 0 {
        return false;
    }
    last_context_tokens.saturating_mul(100)
        >= context_tokens.saturating_mul(AUTO_COMPACT_THRESHOLD_PERCENT)
}

fn compact_summary_from_output(output: &[ModelOutputItem]) -> Option<String> {
    output.iter().rev().find_map(|item| {
        let text = match item {
            ModelOutputItem::Message { text } => text,
            ModelOutputItem::AssistantTurn {
                content: Some(text),
                ..
            } => text,
            ModelOutputItem::AssistantTurn {
                content: None,
                reasoning_content: Some(text),
                ..
            } => text,
            _ => return None,
        };
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_string())
    })
}

fn repair_incomplete_tool_history(history: &mut Vec<ModelInputItem>) {
    use std::collections::HashSet;
    let mut insertions: Vec<(usize, ModelInputItem)> = Vec::new();
    let mut i = 0;
    while i < history.len() {
        let call_ids: Vec<String> = match &history[i] {
            ModelInputItem::AssistantTurn { tool_calls, .. } => {
                tool_calls.iter().map(|tc| tc.call_id.clone()).collect()
            }
            ModelInputItem::FunctionCall { call_id, .. } => {
                vec![call_id.clone()]
            }
            _ => {
                i += 1;
                continue;
            }
        };
        if call_ids.is_empty() {
            i += 1;
            continue;
        }
        let mut answered = HashSet::new();
        let mut last_output_pos = i;
        let mut j = i + 1;
        while j < history.len() {
            if let ModelInputItem::FunctionCallOutput { call_id, .. } = &history[j] {
                if call_ids.iter().any(|id| id == call_id) {
                    answered.insert(call_id.clone());
                }
                last_output_pos = j;
                j += 1;
            } else {
                break;
            }
        }
        for call_id in call_ids {
            if !answered.contains(&call_id) {
                insertions.push((
                    last_output_pos + 1,
                    ModelInputItem::FunctionCallOutput {
                        call_id,
                        output: "error: tool execution interrupted".to_string(),
                    },
                ));
            }
        }
        i = j;
    }
    for (pos, item) in insertions.into_iter().rev() {
        history.insert(pos, item);
    }
}

fn build_compacted_history(history: &[ModelInputItem], summary: &str) -> Vec<ModelInputItem> {
    let mut replacement = recent_user_messages(history, COMPACT_USER_MESSAGE_MAX_CHARS)
        .into_iter()
        .map(ModelInputItem::user_text)
        .collect::<Vec<_>>();
    replacement.push(ModelInputItem::user_text(compact_summary_message(summary)));
    replacement
}

fn compact_summary_message(summary: &str) -> String {
    format!("{}\n{}", COMPACT_SUMMARY_PREFIX, summary.trim())
}

fn is_compact_summary(text: &str) -> bool {
    text.starts_with(COMPACT_SUMMARY_PREFIX)
}

fn recent_user_messages(history: &[ModelInputItem], max_chars: usize) -> Vec<String> {
    let mut selected = Vec::new();
    let mut remaining = max_chars;
    for item in history.iter().rev() {
        if remaining == 0 {
            break;
        }
        let Some(text) = user_message_text(item) else {
            continue;
        };
        if is_compact_summary(text.trim()) {
            continue;
        }
        if text.chars().count() <= remaining {
            selected.push(text.to_string());
            remaining = remaining.saturating_sub(text.chars().count());
        } else {
            selected.push(take_last_chars(text, remaining));
            break;
        }
    }
    selected.reverse();
    selected
}

fn user_message_text(item: &ModelInputItem) -> Option<&str> {
    let ModelInputItem::Message { role, content } = item else {
        return None;
    };
    if role != "user" {
        return None;
    }
    content.iter().find_map(|item| match item {
        ModelContentItem::InputText { text } => Some(text.as_str()),
        ModelContentItem::OutputText { .. } => None,
    })
}

fn take_last_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut chars = text.chars().rev().take(max_chars).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

fn event_agent_id(event: &ServiceEvent) -> Option<AgentId> {
    match &event.kind {
        ServiceEventKind::AgentCreated { agent } | ServiceEventKind::AgentUpdated { agent } => {
            Some(agent.id)
        }
        ServiceEventKind::AgentStatusChanged { agent_id, .. }
        | ServiceEventKind::AgentDeleted { agent_id }
        | ServiceEventKind::TurnStarted { agent_id, .. }
        | ServiceEventKind::TurnCompleted { agent_id, .. }
        | ServiceEventKind::ToolStarted { agent_id, .. }
        | ServiceEventKind::ToolCompleted { agent_id, .. }
        | ServiceEventKind::ContextCompacted { agent_id, .. }
        | ServiceEventKind::AgentMessage { agent_id, .. }
        | ServiceEventKind::TodoListUpdated { agent_id, .. }
        | ServiceEventKind::UserInputRequested { agent_id, .. } => Some(*agent_id),
        ServiceEventKind::TaskCreated { .. }
        | ServiceEventKind::TaskUpdated { .. }
        | ServiceEventKind::TaskDeleted { .. }
        | ServiceEventKind::PlanUpdated { .. }
        | ServiceEventKind::ArtifactCreated { .. } => None,
        ServiceEventKind::Error { agent_id, .. } => *agent_id,
    }
}

const BASE_INSTRUCTIONS: &str = r#"You are Mai, a coding agent running inside a Docker-backed multi-agent service.

General rules:
- You execute all local work inside your own Docker container; do not assume access to a host workspace.
- Use `container_exec` for shell commands inside your container.
- Use `container_cp_upload` and `container_cp_download` for file transfer.
- Use `spawn_agent`, `send_message`, `wait_agent`, `list_agents`, and `close_agent` for multi-agent collaboration.
- Keep each child agent task concrete and bounded. Multiple agents can run in parallel.
- Child agent model selection is controlled by Research Agent settings, falling back to the service default model when unset.
- Use available skills only when explicitly requested by the user or when clearly relevant.
- MCP tools are exposed as ordinary function tools whose names begin with `mcp__`.
- Be concise with final answers and include important file paths or command outputs when they matter.
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{
        ModelConfig, ModelReasoningConfig, ModelReasoningVariant, ProviderConfig, ProviderKind,
        ProvidersConfigRequest,
    };
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn test_model(id: &str) -> ModelConfig {
        ModelConfig {
            id: id.to_string(),
            name: Some(id.to_string()),
            context_tokens: 400_000,
            output_tokens: 128_000,
            supports_tools: true,
            reasoning: Some(openai_reasoning_config(&[
                "minimal", "low", "medium", "high",
            ])),
            options: serde_json::Value::Null,
            headers: Default::default(),
        }
    }

    fn non_reasoning_model(id: &str) -> ModelConfig {
        ModelConfig {
            reasoning: None,
            ..test_model(id)
        }
    }

    fn test_provider() -> ProviderConfig {
        ProviderConfig {
            id: "openai".to_string(),
            kind: ProviderKind::Openai,
            name: "OpenAI".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: Some("secret".to_string()),
            api_key_env: Some("OPENAI_API_KEY".to_string()),
            models: vec![
                test_model("gpt-5.5"),
                test_model("gpt-5.4"),
                non_reasoning_model("gpt-4.1"),
            ],
            default_model: "gpt-5.5".to_string(),
            enabled: true,
        }
    }

    fn deepseek_test_provider() -> ProviderConfig {
        ProviderConfig {
            id: "deepseek".to_string(),
            kind: ProviderKind::Deepseek,
            name: "DeepSeek".to_string(),
            base_url: "https://api.deepseek.com".to_string(),
            api_key: Some("secret".to_string()),
            api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
            models: vec![ModelConfig {
                id: "deepseek-v4-pro".to_string(),
                name: Some("deepseek-v4-pro".to_string()),
                context_tokens: 1_000_000,
                output_tokens: 384_000,
                supports_tools: true,
                reasoning: Some(deepseek_reasoning_config()),
                options: serde_json::Value::Null,
                headers: Default::default(),
            }],
            default_model: "deepseek-v4-pro".to_string(),
            enabled: true,
        }
    }

    fn openai_reasoning_config(variants: &[&str]) -> ModelReasoningConfig {
        ModelReasoningConfig {
            default_variant: Some("medium".to_string()),
            variants: variants
                .iter()
                .map(|id| ModelReasoningVariant {
                    id: (*id).to_string(),
                    label: None,
                    request: json!({
                        "reasoning": {
                            "effort": id,
                        },
                    }),
                })
                .collect(),
        }
    }

    fn deepseek_reasoning_config() -> ModelReasoningConfig {
        ModelReasoningConfig {
            default_variant: Some("high".to_string()),
            variants: ["high", "max"]
                .into_iter()
                .map(|id| ModelReasoningVariant {
                    id: id.to_string(),
                    label: None,
                    request: json!({
                        "thinking": {
                            "type": "enabled",
                        },
                        "reasoning_effort": id,
                    }),
                })
                .collect(),
        }
    }

    fn alt_test_provider() -> ProviderConfig {
        ProviderConfig {
            id: "alt".to_string(),
            kind: ProviderKind::Openai,
            name: "Alt".to_string(),
            base_url: "https://alt.example/v1".to_string(),
            api_key: Some("secret".to_string()),
            api_key_env: None,
            models: vec![test_model("alt-default"), test_model("alt-research")],
            default_model: "alt-default".to_string(),
            enabled: true,
        }
    }

    async fn start_mock_responses(responses: Vec<Value>) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock server addr");
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_responses = Arc::clone(&responses);
        let server_requests = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let responses = Arc::clone(&server_responses);
                let requests = Arc::clone(&server_requests);
                tokio::spawn(async move {
                    let request = read_mock_request(&mut stream).await;
                    requests.lock().await.push(request);
                    let response = responses.lock().await.pop_front().unwrap_or_else(|| {
                        json!({
                            "id": "resp_empty",
                            "output": [],
                            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
                        })
                    });
                    if response
                        .get("__close_without_response")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        return;
                    }
                    write_mock_response(&mut stream, response).await;
                });
            }
        });
        (format!("http://{addr}"), requests)
    }

    async fn read_mock_request(stream: &mut tokio::net::TcpStream) -> Value {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk).await.expect("read request");
            assert!(read > 0, "mock request closed before headers");
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(header_end) = find_header_end(&buffer) {
                break header_end;
            }
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]);
        let content_length = content_length(&headers);
        while buffer.len() < header_end + content_length {
            let read = stream.read(&mut chunk).await.expect("read request body");
            assert!(read > 0, "mock request closed before body");
            buffer.extend_from_slice(&chunk[..read]);
        }
        serde_json::from_slice(&buffer[header_end..header_end + content_length])
            .expect("request json")
    }

    async fn write_mock_response(stream: &mut tokio::net::TcpStream, response: Value) {
        let body = serde_json::to_string(&response).expect("response json");
        let reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(reply.as_bytes())
            .await
            .expect("write response");
    }

    fn find_header_end(buffer: &[u8]) -> Option<usize> {
        buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
    }

    fn content_length(headers: &str) -> usize {
        headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or_default()
    }

    fn compact_test_provider(base_url: String) -> ProviderConfig {
        let mut model = test_model("mock-model");
        model.context_tokens = 100;
        model.output_tokens = 32;
        ProviderConfig {
            id: "mock".to_string(),
            kind: ProviderKind::Openai,
            name: "Mock".to_string(),
            base_url,
            api_key: Some("secret".to_string()),
            api_key_env: None,
            models: vec![model],
            default_model: "mock-model".to_string(),
            enabled: true,
        }
    }

    fn test_agent_summary(agent_id: AgentId, container_id: Option<&str>) -> AgentSummary {
        test_agent_summary_with_parent(agent_id, None, container_id)
    }

    fn test_agent_summary_with_parent(
        agent_id: AgentId,
        parent_id: Option<AgentId>,
        container_id: Option<&str>,
    ) -> AgentSummary {
        let timestamp = now();
        AgentSummary {
            id: agent_id,
            parent_id,
            task_id: None,
            role: None,
            name: "compact-agent".to_string(),
            status: AgentStatus::Idle,
            container_id: container_id.map(ToOwned::to_owned),
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "mock".to_string(),
            provider_name: "Mock".to_string(),
            model: "mock-model".to_string(),
            reasoning_effort: Some("medium".to_string()),
            created_at: timestamp,
            updated_at: timestamp,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        }
    }

    fn test_agent_summary_at(
        agent_id: AgentId,
        parent_id: Option<AgentId>,
        created_at: chrono::DateTime<Utc>,
    ) -> AgentSummary {
        AgentSummary {
            created_at,
            updated_at: created_at,
            ..test_agent_summary_with_parent(agent_id, parent_id, None)
        }
    }

    async fn test_store(dir: &tempfile::TempDir) -> Arc<ConfigStore> {
        Arc::new(
            ConfigStore::open_with_config_path(
                &dir.path().join("runtime.sqlite3"),
                &dir.path().join("config.toml"),
            )
            .await
            .expect("open store"),
        )
    }

    async fn save_agent_with_session(store: &ConfigStore, summary: &AgentSummary) {
        store.save_agent(summary, None).await.expect("save agent");
        save_test_session(store, summary.id, Uuid::new_v4()).await;
    }

    async fn test_runtime(dir: &tempfile::TempDir, store: Arc<ConfigStore>) -> Arc<AgentRuntime> {
        AgentRuntime::new(
            DockerClient::new_with_binary("unused", fake_docker_path(dir)),
            ResponsesClient::new(),
            store,
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime")
    }

    fn fake_docker_path(dir: &tempfile::TempDir) -> String {
        let path = dir.path().join("fake-docker.sh");
        let log_path = fake_docker_log_path(dir);
        let script = format!(
            r#"#!/bin/sh
LOG={}
case "$1" in
  ps)
    exit 0
    ;;
  commit)
    echo "$*" >> "$LOG"
    echo "sha256:snapshot"
    exit 0
    ;;
  create)
    echo "$*" >> "$LOG"
    echo "created-container"
    exit 0
    ;;
  rm|rmi|start|exec)
    echo "$*" >> "$LOG"
    exit 0
    ;;
  *)
    echo "$*" >> "$LOG"
    exit 0
    ;;
esac
"#,
            test_shell_quote(&log_path.to_string_lossy())
        );
        std::fs::write(&path, script).expect("write fake docker");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path)
                .expect("fake docker metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).expect("chmod fake docker");
        }
        path.to_string_lossy().to_string()
    }

    fn fake_docker_log_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("fake-docker.log")
    }

    fn fake_docker_log(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(fake_docker_log_path(dir)).unwrap_or_default()
    }

    fn test_shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    async fn save_test_session(store: &ConfigStore, agent_id: AgentId, session_id: SessionId) {
        let timestamp = now();
        store
            .save_agent_session(
                agent_id,
                &AgentSessionSummary {
                    id: session_id,
                    title: "Chat 1".to_string(),
                    created_at: timestamp,
                    updated_at: timestamp,
                    message_count: 0,
                },
            )
            .await
            .expect("save session");
    }

    #[test]
    fn extracts_skill_mentions() {
        assert_eq!(
            extract_skill_mentions("please use $rust-dev, then $doc."),
            vec!["rust-dev", "doc"]
        );
    }

    #[test]
    fn agent_status_allows_new_turn_after_completion() {
        assert!(AgentStatus::Completed.can_start_turn());
        assert!(!AgentStatus::RunningTurn.can_start_turn());
    }

    #[tokio::test]
    async fn create_task_persists_planner_metadata_and_rejects_extra_sessions() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        let task = runtime
            .create_task(Some("Build task UI".to_string()), None, Some("ubuntu:latest".to_string()))
            .await
            .expect("create task");

        assert_eq!(task.status, TaskStatus::Planning);
        assert_eq!(task.plan_status, PlanStatus::Missing);
        let detail = runtime.get_task(task.id, None).await.expect("task detail");
        assert_eq!(detail.agents.len(), 1);
        assert_eq!(detail.selected_agent.summary.role, Some(AgentRole::Planner));
        assert_eq!(detail.selected_agent.summary.task_id, Some(task.id));
        assert_eq!(detail.selected_agent.sessions.len(), 1);
        assert_eq!(detail.selected_agent.sessions[0].title, "Task");
        assert!(runtime
            .create_session(detail.selected_agent.summary.id)
            .await
            .is_err());

        let snapshot = store.load_runtime_snapshot(20).await.expect("snapshot");
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.tasks[0].summary.id, task.id);
        let planner = snapshot
            .agents
            .iter()
            .find(|agent| agent.summary.id == task.planner_agent_id)
            .expect("planner");
        assert_eq!(planner.summary.task_id, Some(task.id));
        assert_eq!(planner.summary.role, Some(AgentRole::Planner));
    }

    #[tokio::test]
    async fn task_plan_tool_requires_planner_and_updates_task_status() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let task = runtime
            .create_task(Some("Plan me".to_string()), None, Some("ubuntu:latest".to_string()))
            .await
            .expect("create task");

        let output = runtime
            .execute_tool_for_test(
                task.planner_agent_id,
                "save_task_plan",
                json!({
                    "title": "Implementation plan",
                    "markdown": "# Plan\n\nShip it carefully."
                }),
            )
            .await
            .expect("save plan");
        assert!(output.success);
        let detail = runtime.get_task(task.id, None).await.expect("task detail");
        assert_eq!(detail.summary.status, TaskStatus::AwaitingApproval);
        assert_eq!(detail.plan.status, PlanStatus::Ready);
        assert_eq!(detail.plan.version, 1);
        assert_eq!(detail.plan.title.as_deref(), Some("Implementation plan"));

        let explorer = runtime
            .spawn_task_role_agent(
                task.planner_agent_id,
                AgentRole::Explorer,
                Some("Explorer".to_string()),
            )
            .await
            .expect("explorer");
        let rejected = runtime
            .execute_tool_for_test(
                explorer.id,
                "save_task_plan",
                json!({
                    "title": "Nope",
                    "markdown": "Only planner may do this."
                }),
            )
            .await;
        assert!(rejected.is_err());
    }

    #[tokio::test]
    async fn approving_task_without_ready_plan_fails() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let runtime = test_runtime(&dir, store).await;
        let task = runtime
            .create_task(Some("Needs plan".to_string()), None, Some("ubuntu:latest".to_string()))
            .await
            .expect("create task");

        assert!(runtime.approve_task_plan(task.id).await.is_err());
    }

    #[test]
    fn descendant_delete_order_deletes_children_before_parents() {
        let parent = Uuid::new_v4();
        let older_child = Uuid::new_v4();
        let younger_child = Uuid::new_v4();
        let grandchild = Uuid::new_v4();
        let unrelated = Uuid::new_v4();
        let base = now();
        let summaries = vec![
            test_agent_summary_at(parent, None, base),
            test_agent_summary_at(
                younger_child,
                Some(parent),
                base + chrono::Duration::seconds(2),
            ),
            test_agent_summary_at(
                older_child,
                Some(parent),
                base + chrono::Duration::seconds(1),
            ),
            test_agent_summary_at(
                grandchild,
                Some(older_child),
                base + chrono::Duration::seconds(3),
            ),
            test_agent_summary_at(unrelated, None, base + chrono::Duration::seconds(4)),
        ];

        assert_eq!(
            descendant_delete_order_from_summaries(parent, &summaries),
            vec![grandchild, older_child, younger_child, parent]
        );
    }

    #[tokio::test]
    async fn delete_parent_cascades_to_children_and_grandchildren() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let sibling = Uuid::new_v4();
        let grandchild = Uuid::new_v4();
        save_agent_with_session(
            &store,
            &test_agent_summary(parent, Some("parent-container")),
        )
        .await;
        save_agent_with_session(
            &store,
            &test_agent_summary_with_parent(child, Some(parent), Some("child-container")),
        )
        .await;
        save_agent_with_session(
            &store,
            &test_agent_summary_with_parent(sibling, Some(parent), Some("sibling-container")),
        )
        .await;
        save_agent_with_session(
            &store,
            &test_agent_summary_with_parent(grandchild, Some(child), Some("grandchild-container")),
        )
        .await;
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        runtime.delete_agent(parent).await.expect("delete parent");

        assert!(runtime.list_agents().await.is_empty());
        assert!(
            store
                .load_runtime_snapshot(RECENT_EVENT_LIMIT)
                .await
                .expect("snapshot")
                .agents
                .is_empty()
        );
        let events = runtime.recent_events.lock().await;
        let deleted = events
            .iter()
            .filter_map(|event| match event.kind {
                ServiceEventKind::AgentDeleted { agent_id } => Some(agent_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(deleted, vec![grandchild, child, sibling, parent]);
    }

    #[tokio::test]
    async fn delete_child_keeps_parent_and_sibling() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let sibling = Uuid::new_v4();
        save_agent_with_session(
            &store,
            &test_agent_summary(parent, Some("parent-container")),
        )
        .await;
        save_agent_with_session(
            &store,
            &test_agent_summary_with_parent(child, Some(parent), Some("child-container")),
        )
        .await;
        save_agent_with_session(
            &store,
            &test_agent_summary_with_parent(sibling, Some(parent), Some("sibling-container")),
        )
        .await;
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        runtime.delete_agent(child).await.expect("delete child");

        let remaining = runtime
            .list_agents()
            .await
            .into_iter()
            .map(|agent| agent.id)
            .collect::<HashSet<_>>();
        assert_eq!(remaining, HashSet::from([parent, sibling]));
    }

    #[test]
    fn auto_compact_threshold_uses_last_context_tokens() {
        assert!(!should_auto_compact(0, 100));
        assert!(!should_auto_compact(79, 100));
        assert!(should_auto_compact(80, 100));
        assert!(should_auto_compact(330_000, 400_000));
        assert!(!should_auto_compact(80, 0));
    }

    #[test]
    fn compact_summary_uses_last_non_empty_assistant_output() {
        let output = vec![
            ModelOutputItem::Message {
                text: "first".to_string(),
            },
            ModelOutputItem::AssistantTurn {
                content: Some("  second  ".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
            },
        ];

        assert_eq!(
            compact_summary_from_output(&output).as_deref(),
            Some("second")
        );
        assert_eq!(compact_summary_from_output(&[]), None);
    }

    #[test]
    fn repair_adds_missing_tool_outputs_for_assistant_turn() {
        let mut history = vec![
            ModelInputItem::user_text("do something"),
            ModelInputItem::AssistantTurn {
                content: None,
                reasoning_content: None,
                tool_calls: vec![ModelToolCall {
                    call_id: "call_1".to_string(),
                    name: "container_exec".to_string(),
                    arguments: "{}".to_string(),
                }],
            },
        ];
        repair_incomplete_tool_history(&mut history);
        assert_eq!(history.len(), 3);
        assert!(matches!(
            &history[2],
            ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_1"
        ));
    }

    #[test]
    fn repair_adds_missing_tool_outputs_for_partial_results() {
        let mut history = vec![
            ModelInputItem::AssistantTurn {
                content: None,
                reasoning_content: None,
                tool_calls: vec![
                    ModelToolCall {
                        call_id: "call_1".to_string(),
                        name: "container_exec".to_string(),
                        arguments: "{}".to_string(),
                    },
                    ModelToolCall {
                        call_id: "call_2".to_string(),
                        name: "wait_agent".to_string(),
                        arguments: "{}".to_string(),
                    },
                ],
            },
            ModelInputItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: "done".to_string(),
            },
        ];
        repair_incomplete_tool_history(&mut history);
        assert_eq!(history.len(), 3);
        assert!(matches!(
            &history[2],
            ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_2"
        ));
    }

    #[test]
    fn repair_adds_missing_tool_outputs_for_function_call() {
        let mut history = vec![
            ModelInputItem::FunctionCall {
                call_id: "call_a".to_string(),
                name: "container_exec".to_string(),
                arguments: "{}".to_string(),
            },
        ];
        repair_incomplete_tool_history(&mut history);
        assert_eq!(history.len(), 2);
        assert!(matches!(
            &history[1],
            ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_a"
        ));
    }

    #[test]
    fn repair_does_nothing_for_complete_history() {
        let mut history = vec![
            ModelInputItem::user_text("run"),
            ModelInputItem::FunctionCall {
                call_id: "call_1".to_string(),
                name: "container_exec".to_string(),
                arguments: "{}".to_string(),
            },
            ModelInputItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: "ok".to_string(),
            },
            ModelInputItem::Message {
                role: "assistant".to_string(),
                content: vec![ModelContentItem::OutputText {
                    text: "done".to_string(),
                }],
            },
        ];
        repair_incomplete_tool_history(&mut history);
        assert_eq!(history.len(), 4);
    }

    #[test]
    fn repair_does_nothing_for_empty_history() {
        let mut history: Vec<ModelInputItem> = vec![];
        repair_incomplete_tool_history(&mut history);
        assert!(history.is_empty());
    }

    #[test]
    fn repair_inserts_before_user_message() {
        let mut history = vec![
            ModelInputItem::user_text("do something"),
            ModelInputItem::AssistantTurn {
                content: None,
                reasoning_content: None,
                tool_calls: vec![ModelToolCall {
                    call_id: "call_1".to_string(),
                    name: "container_exec".to_string(),
                    arguments: "{}".to_string(),
                }],
            },
            ModelInputItem::user_text("继续"),
        ];
        repair_incomplete_tool_history(&mut history);
        // Should be: user, AssistantTurn, FunctionCallOutput, user("继续")
        assert_eq!(history.len(), 4);
        assert!(matches!(
            &history[2],
            ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_1"
        ));
        assert!(matches!(
            &history[3],
            ModelInputItem::Message { role, .. } if role == "user"
        ));
    }

    #[test]
    fn repair_inserts_partial_before_user_message() {
        let mut history = vec![
            ModelInputItem::AssistantTurn {
                content: None,
                reasoning_content: None,
                tool_calls: vec![
                    ModelToolCall {
                        call_id: "call_1".to_string(),
                        name: "exec".to_string(),
                        arguments: "{}".to_string(),
                    },
                    ModelToolCall {
                        call_id: "call_2".to_string(),
                        name: "read".to_string(),
                        arguments: "{}".to_string(),
                    },
                ],
            },
            ModelInputItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: "ok".to_string(),
            },
            ModelInputItem::user_text("继续"),
        ];
        repair_incomplete_tool_history(&mut history);
        // Should be: AssistantTurn, FCO(call_1), FCO(call_2), user("继续")
        assert_eq!(history.len(), 4);
        assert!(matches!(
            &history[2],
            ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_2"
        ));
        assert!(matches!(
            &history[3],
            ModelInputItem::Message { role, .. } if role == "user"
        ));
    }

    #[test]
    fn compacted_history_keeps_recent_user_messages_and_summary_only() {
        let history = vec![
            ModelInputItem::user_text("first user"),
            ModelInputItem::assistant_text("assistant old"),
            ModelInputItem::user_text(compact_summary_message("old summary")),
            ModelInputItem::FunctionCall {
                call_id: "call_1".to_string(),
                name: "container_exec".to_string(),
                arguments: "{}".to_string(),
            },
            ModelInputItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: "{}".to_string(),
            },
            ModelInputItem::user_text("second user"),
        ];

        let compacted = build_compacted_history(&history, "new summary");
        assert_eq!(compacted.len(), 3);
        assert!(matches!(
            &compacted[0],
            ModelInputItem::Message { content, .. }
                if matches!(&content[0], ModelContentItem::InputText { text } if text == "first user")
        ));
        assert!(matches!(
            &compacted[1],
            ModelInputItem::Message { content, .. }
                if matches!(&content[0], ModelContentItem::InputText { text } if text == "second user")
        ));
        assert!(matches!(
            &compacted[2],
            ModelInputItem::Message { content, .. }
                if matches!(&content[0], ModelContentItem::InputText { text } if text.contains("new summary") && is_compact_summary(text))
        ));
    }

    #[test]
    fn recent_user_messages_truncates_from_oldest_side() {
        let history = vec![
            ModelInputItem::user_text("abcdef"),
            ModelInputItem::user_text("ghij"),
        ];

        assert_eq!(recent_user_messages(&history, 7), vec!["def", "ghij"]);
    }

    #[tokio::test]
    async fn restores_persisted_agents_and_continues_event_sequence() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store");
        let agent_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        let timestamp = now();
        let summary = AgentSummary {
            id: agent_id,
            parent_id: None,
            task_id: None,
            role: None,
            name: "restored".to_string(),
            status: AgentStatus::RunningTurn,
            container_id: Some("old-container".to_string()),
            docker_image: "ghcr.io/rcore-os/tgoskits-container:latest".to_string(),
            provider_id: "openai".to_string(),
            provider_name: "OpenAI".to_string(),
            model: "gpt-5.2".to_string(),
            reasoning_effort: None,
            created_at: timestamp,
            updated_at: timestamp,
            current_turn: Some(turn_id),
            last_error: None,
            token_usage: TokenUsage::default(),
        };
        let message = AgentMessage {
            role: MessageRole::User,
            content: "hello".to_string(),
            created_at: timestamp,
        };
        store
            .save_agent(&summary, Some("system"))
            .await
            .expect("save agent");
        store
            .save_agent_session(
                agent_id,
                &AgentSessionSummary {
                    id: session_id,
                    title: "Chat 1".to_string(),
                    created_at: timestamp,
                    updated_at: timestamp,
                    message_count: 0,
                },
            )
            .await
            .expect("save session");
        store
            .append_agent_message(agent_id, session_id, 0, &message)
            .await
            .expect("save message");
        store
            .append_agent_history_item(agent_id, session_id, 0, &ModelInputItem::user_text("hello"))
            .await
            .expect("save history");
        store
            .append_service_event(&ServiceEvent {
                sequence: 41,
                timestamp,
                kind: ServiceEventKind::AgentMessage {
                    agent_id,
                    session_id: Some(session_id),
                    turn_id: Some(turn_id),
                    role: MessageRole::User,
                    content: "hello".to_string(),
                },
            })
            .await
            .expect("save event");
        drop(store);

        let store = Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("reopen store"),
        );
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            store,
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");

        let agents = runtime.list_agents().await;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].status, AgentStatus::Failed);
        assert_eq!(agents[0].container_id.as_deref(), Some("old-container"));
        assert_eq!(agents[0].current_turn, None);
        assert_eq!(
            agents[0].last_error.as_deref(),
            Some("interrupted by server restart")
        );

        let detail = runtime
            .get_agent(agent_id, Some(session_id))
            .await
            .expect("detail");
        assert_eq!(detail.selected_session_id, session_id);
        assert_eq!(detail.sessions.len(), 1);
        assert_eq!(detail.messages.len(), 1);
        assert_eq!(detail.messages[0].content, "hello");

        runtime
            .publish(ServiceEventKind::AgentStatusChanged {
                agent_id,
                status: AgentStatus::Failed,
            })
            .await;
        let events = runtime.recent_events.lock().await;
        assert_eq!(events.back().expect("event").sequence, 42);
    }

    #[tokio::test]
    async fn wait_agent_tool_returns_final_assistant_response() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let parent_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let child_session_id = Uuid::new_v4();
        let timestamp = now();
        let parent = test_agent_summary(parent_id, Some("parent-container"));
        let child = AgentSummary {
            id: child_id,
            parent_id: Some(parent_id),
            task_id: None,
            role: Some(AgentRole::Explorer),
            name: "Explorer".to_string(),
            status: AgentStatus::Completed,
            container_id: Some("child-container".to_string()),
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "mock".to_string(),
            provider_name: "Mock".to_string(),
            model: "mock-model".to_string(),
            reasoning_effort: Some("medium".to_string()),
            created_at: timestamp,
            updated_at: timestamp,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        };
        store.save_agent(&parent, None).await.expect("save parent");
        save_test_session(&store, parent_id, Uuid::new_v4()).await;
        store.save_agent(&child, None).await.expect("save child");
        store
            .save_agent_session(
                child_id,
                &AgentSessionSummary {
                    id: child_session_id,
                    title: "Task".to_string(),
                    created_at: timestamp,
                    updated_at: timestamp,
                    message_count: 0,
                },
            )
            .await
            .expect("save child session");
        store
            .append_agent_message(
                child_id,
                child_session_id,
                0,
                &AgentMessage {
                    role: MessageRole::User,
                    content: "Explore auth code".to_string(),
                    created_at: timestamp,
                },
            )
            .await
            .expect("save user message");
        store
            .append_agent_message(
                child_id,
                child_session_id,
                1,
                &AgentMessage {
                    role: MessageRole::Assistant,
                    content: "Explorer conclusion: auth lives in crates/auth.".to_string(),
                    created_at: timestamp,
                },
            )
            .await
            .expect("save assistant message");
        let runtime = test_runtime(&dir, store).await;

        let output = runtime
            .execute_tool_for_test(
                parent_id,
                "wait_agent",
                json!({
                    "agent_id": child_id.to_string(),
                    "timeout_secs": 1
                }),
            )
            .await
            .expect("wait agent");
        assert!(output.success);
        let value: Value = serde_json::from_str(&output.output).expect("wait output json");
        assert_eq!(
            value["final_response"].as_str(),
            Some("Explorer conclusion: auth lives in crates/auth.")
        );
        assert_eq!(value["recent_messages"].as_array().expect("messages").len(), 2);
        assert_eq!(value["agent"]["id"].as_str(), Some(child_id.to_string().as_str()));
        assert!(matches!(
            runtime.agent(child_id).await,
            Err(RuntimeError::AgentNotFound(id)) if id == child_id
        ));
        assert!(runtime
            .list_agents()
            .await
            .iter()
            .all(|agent| agent.id != child_id));
    }

    #[tokio::test]
    async fn tool_trace_returns_full_history_with_event_metadata() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store");
        let agent_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        let timestamp = now();
        let summary = AgentSummary {
            id: agent_id,
            parent_id: None,
            task_id: None,
            role: None,
            name: "trace".to_string(),
            status: AgentStatus::Completed,
            container_id: None,
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "openai".to_string(),
            provider_name: "OpenAI".to_string(),
            model: "gpt-5.2".to_string(),
            reasoning_effort: None,
            created_at: timestamp,
            updated_at: timestamp,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        };
        store.save_agent(&summary, None).await.expect("save agent");
        store
            .save_agent_session(
                agent_id,
                &AgentSessionSummary {
                    id: session_id,
                    title: "Chat 1".to_string(),
                    created_at: timestamp,
                    updated_at: timestamp,
                    message_count: 0,
                },
            )
            .await
            .expect("save session");
        store
            .append_agent_history_item(
                agent_id,
                session_id,
                0,
                &ModelInputItem::FunctionCall {
                    call_id: "call_1".to_string(),
                    name: "container_exec".to_string(),
                    arguments: r#"{"command":"printf hello","cwd":"/workspace"}"#.to_string(),
                },
            )
            .await
            .expect("save call");
        store
            .append_agent_history_item(
                agent_id,
                session_id,
                1,
                &ModelInputItem::FunctionCallOutput {
                    call_id: "call_1".to_string(),
                    output: r#"{"status":0,"stdout":"hello","stderr":""}"#.to_string(),
                },
            )
            .await
            .expect("save output");
        store
            .append_service_event(&ServiceEvent {
                sequence: 9,
                timestamp,
                kind: ServiceEventKind::ToolCompleted {
                    agent_id,
                    session_id: Some(session_id),
                    turn_id,
                    call_id: "call_1".to_string(),
                    tool_name: "container_exec".to_string(),
                    success: true,
                    output_preview: "hello".to_string(),
                    duration_ms: Some(27),
                },
            })
            .await
            .expect("save event");
        drop(store);

        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            Arc::new(
                ConfigStore::open_with_config_path(&db_path, &config_path)
                    .await
                    .expect("reopen store"),
            ),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");

        let trace = runtime
            .tool_trace(agent_id, Some(session_id), "call_1".to_string())
            .await
            .expect("trace");
        assert_eq!(trace.tool_name, "container_exec");
        assert_eq!(trace.arguments["command"], "printf hello");
        assert_eq!(trace.output, r#"{"status":0,"stdout":"hello","stderr":""}"#);
        assert!(trace.success);
        assert_eq!(trace.duration_ms, Some(27));
    }

    #[tokio::test]
    async fn tool_trace_finds_calls_stored_inside_assistant_turns() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store");
        let agent_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let timestamp = now();
        let summary = AgentSummary {
            id: agent_id,
            parent_id: None,
            task_id: None,
            role: None,
            name: "assistant-turn-trace".to_string(),
            status: AgentStatus::Completed,
            container_id: None,
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "openai".to_string(),
            provider_name: "OpenAI".to_string(),
            model: "gpt-5.2".to_string(),
            reasoning_effort: None,
            created_at: timestamp,
            updated_at: timestamp,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        };
        store.save_agent(&summary, None).await.expect("save agent");
        store
            .save_agent_session(
                agent_id,
                &AgentSessionSummary {
                    id: session_id,
                    title: "Chat 1".to_string(),
                    created_at: timestamp,
                    updated_at: timestamp,
                    message_count: 0,
                },
            )
            .await
            .expect("save session");
        store
            .append_agent_history_item(
                agent_id,
                session_id,
                0,
                &ModelInputItem::AssistantTurn {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![ModelToolCall {
                        call_id: "call_nested".to_string(),
                        name: "container_exec".to_string(),
                        arguments: r#"{"command":"pwd"}"#.to_string(),
                    }],
                },
            )
            .await
            .expect("save assistant turn");
        store
            .append_agent_history_item(
                agent_id,
                session_id,
                1,
                &ModelInputItem::FunctionCallOutput {
                    call_id: "call_nested".to_string(),
                    output: r#"{"status":0,"stdout":"/workspace\n","stderr":""}"#.to_string(),
                },
            )
            .await
            .expect("save output");
        drop(store);

        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            Arc::new(
                ConfigStore::open_with_config_path(&db_path, &config_path)
                    .await
                    .expect("reopen store"),
            ),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");

        let trace = runtime
            .tool_trace(agent_id, Some(session_id), "call_nested".to_string())
            .await
            .expect("trace");
        assert_eq!(trace.tool_name, "container_exec");
        assert_eq!(trace.arguments["command"], "pwd");
        assert_eq!(
            trace.output,
            r#"{"status":0,"stdout":"/workspace\n","stderr":""}"#
        );
        assert!(trace.success);
    }

    #[tokio::test]
    async fn auto_compact_failure_keeps_original_history() {
        let (base_url, _requests) = start_mock_responses(vec![json!({
            "id": "compact_empty",
            "output": [],
            "usage": { "input_tokens": 50, "output_tokens": 1, "total_tokens": 51 }
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("open store"),
        );
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![compact_test_provider(base_url)],
                default_provider_id: Some("mock".to_string()),
            })
            .await
            .expect("save providers");
        let agent_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        store
            .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
            .await
            .expect("save agent");
        save_test_session(&store, agent_id, session_id).await;
        let original_history = [
            ModelInputItem::user_text("original request"),
            ModelInputItem::assistant_text("original answer"),
        ];
        for (position, item) in original_history.iter().enumerate() {
            store
                .append_agent_history_item(agent_id, session_id, position, item)
                .await
                .expect("append history");
        }
        store
            .save_session_context_tokens(agent_id, session_id, 80)
            .await
            .expect("save tokens");
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");
        let agent = runtime.agent(agent_id).await.expect("agent");

        let compacted = runtime
            .maybe_auto_compact(&agent, agent_id, session_id, Uuid::new_v4())
            .await;

        assert!(matches!(compacted, Err(RuntimeError::InvalidInput(_))));
        let history = store
            .load_runtime_snapshot(10)
            .await
            .expect("snapshot")
            .agents[0]
            .sessions[0]
            .history
            .clone();
        assert_eq!(history.len(), original_history.len());
        assert!(matches!(
            &history[0],
            ModelInputItem::Message { content, .. }
                if matches!(&content[0], ModelContentItem::InputText { text } if text == "original request")
        ));
        assert!(matches!(
            &history[1],
            ModelInputItem::Message { content, .. }
                if matches!(&content[0], ModelContentItem::OutputText { text } if text == "original answer")
        ));
        assert_eq!(
            store
                .load_runtime_snapshot(10)
                .await
                .expect("snapshot")
                .agents[0]
                .sessions[0]
                .last_context_tokens,
            Some(80)
        );
    }

    #[tokio::test]
    async fn auto_compact_runs_after_tool_output_before_next_model_request() {
        let (base_url, requests) = start_mock_responses(vec![
            json!({
                "id": "first",
                "output": [{
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "unknown_tool",
                    "arguments": "{}"
                }],
                "usage": { "input_tokens": 78, "output_tokens": 2, "total_tokens": 80 }
            }),
            json!({
                "id": "compact",
                "output": [{
                    "type": "message",
                    "content": [{ "type": "output_text", "text": "summary after tool output" }]
                }],
                "usage": { "input_tokens": 20, "output_tokens": 5, "total_tokens": 25 }
            }),
            json!({
                "id": "second",
                "output": [{
                    "type": "message",
                    "content": [{ "type": "output_text", "text": "final answer" }]
                }],
                "usage": { "input_tokens": 40, "output_tokens": 4, "total_tokens": 44 }
            }),
        ])
        .await;
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("open store"),
        );
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![compact_test_provider(base_url)],
                default_provider_id: Some("mock".to_string()),
            })
            .await
            .expect("save providers");
        let agent_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        store
            .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
            .await
            .expect("save agent");
        save_test_session(&store, agent_id, session_id).await;
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");
        let agent = runtime.agent(agent_id).await.expect("agent");
        *agent.container.write().await = Some(ContainerHandle {
            id: "container-1".to_string(),
            name: "container-1".to_string(),
            image: "unused".to_string(),
        });

        runtime
            .run_turn_inner(
                agent_id,
                session_id,
                Uuid::new_v4(),
                "please use a tool".to_string(),
                Vec::new(),
            )
            .await
            .expect("turn");

        let requests = requests.lock().await.clone();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0]["tools"].as_array().expect("first tools").len(),
            13
        );
        assert!(
            requests[1].get("tools").is_none(),
            "compact request should not send tools"
        );
        assert_eq!(
            requests[2]["tools"].as_array().expect("second tools").len(),
            13
        );
        let compact_input = requests[1]["input"].as_array().expect("compact input");
        assert!(matches!(
            compact_input.last(),
            Some(value) if value["content"][0]["text"].as_str().is_some_and(|text| text.contains("CONTEXT CHECKPOINT COMPACTION"))
        ));

        let snapshot = store.load_runtime_snapshot(20).await.expect("snapshot");
        let session = &snapshot.agents[0].sessions[0];
        assert_eq!(session.last_context_tokens, Some(44));
        assert!(session.history.iter().any(|item| matches!(
            item,
            ModelInputItem::Message { role, content }
                if role == "user"
                    && matches!(&content[0], ModelContentItem::InputText { text } if is_compact_summary(text) && text.contains("summary after tool output"))
        )));
        assert!(
            !session
                .history
                .iter()
                .any(|item| matches!(item, ModelInputItem::FunctionCallOutput { .. }))
        );
        assert_eq!(session.history.last().and_then(user_message_text), None);
        assert!(matches!(
            session.history.last(),
            Some(ModelInputItem::Message { role, content })
                if role == "assistant"
                    && matches!(&content[0], ModelContentItem::OutputText { text } if text == "final answer")
        ));
        assert!(
            runtime
                .recent_events
                .lock()
                .await
                .iter()
                .any(|event| matches!(
                    event.kind,
                    ServiceEventKind::ContextCompacted {
                        tokens_before: 80,
                        ..
                    }
                ))
        );
    }

    #[tokio::test]
    async fn model_failure_after_tool_keeps_tool_success_event_separate() {
        let (base_url, _requests) = start_mock_responses(vec![
            json!({
                "id": "first",
                "output": [{
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "list_agents",
                    "arguments": "{}"
                }],
                "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
            }),
            json!({ "__close_without_response": true }),
        ])
        .await;
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("open store"),
        );
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![compact_test_provider(base_url)],
                default_provider_id: Some("mock".to_string()),
            })
            .await
            .expect("save providers");
        let agent_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        store
            .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
            .await
            .expect("save agent");
        save_test_session(&store, agent_id, session_id).await;
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");
        let agent = runtime.agent(agent_id).await.expect("agent");
        *agent.container.write().await = Some(ContainerHandle {
            id: "container-1".to_string(),
            name: "container-1".to_string(),
            image: "unused".to_string(),
        });

        let result = runtime
            .run_turn_inner(
                agent_id,
                session_id,
                Uuid::new_v4(),
                "please list agents".to_string(),
                Vec::new(),
            )
            .await;

        assert!(result.is_err());
        let events = runtime.recent_events.lock().await;
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            ServiceEventKind::ToolCompleted {
                call_id,
                tool_name,
                success: true,
                ..
            } if call_id == "call_1" && tool_name == "list_agents"
        )));
    }

    #[tokio::test]
    async fn sessions_are_created_and_selected_independently() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("open store"),
        );
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");

        let agent = runtime
            .create_agent(CreateAgentRequest {
                name: Some("chat-agent".to_string()),
                provider_id: Some("openai".to_string()),
                model: Some("gpt-5.5".to_string()),
                reasoning_effort: Some("high".to_string()),
                docker_image: None,
                parent_id: None,
                system_prompt: None,
            })
            .await;
        assert!(
            agent.is_err(),
            "unused docker cannot start, but agent is persisted"
        );
        let agent = runtime.list_agents().await[0].clone();
        assert_eq!(agent.reasoning_effort, Some("high".to_string()));
        assert_eq!(agent.docker_image, "unused");
        let first = runtime
            .get_agent(agent.id, None)
            .await
            .expect("first detail");
        assert_eq!(first.sessions.len(), 1);
        assert_eq!(first.sessions[0].title, "Chat 1");

        let second = runtime.create_session(agent.id).await.expect("new session");
        assert_eq!(second.title, "Chat 2");
        let detail = runtime
            .get_agent(agent.id, Some(second.id))
            .await
            .expect("second detail");
        assert_eq!(detail.sessions.len(), 2);
        assert_eq!(detail.selected_session_id, second.id);
        assert!(detail.messages.is_empty());
        assert_eq!(
            detail
                .context_usage
                .as_ref()
                .map(|usage| usage.context_tokens),
            Some(400_000)
        );
        assert_eq!(
            detail
                .context_usage
                .as_ref()
                .map(|usage| usage.threshold_percent),
            Some(80)
        );

        let reopened = store.load_runtime_snapshot(10).await.expect("snapshot");
        assert_eq!(reopened.agents[0].sessions.len(), 2);
        assert_eq!(
            reopened.agents[0].summary.reasoning_effort,
            Some("high".to_string())
        );
    }

    #[tokio::test]
    async fn agent_detail_uses_deepseek_v4_context_tokens() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("open store"),
        );
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![deepseek_test_provider()],
                default_provider_id: Some("deepseek".to_string()),
            })
            .await
            .expect("save providers");
        let agent_id = Uuid::new_v4();
        let timestamp = now();
        store
            .save_agent(
                &AgentSummary {
                    id: agent_id,
                    parent_id: None,
                    task_id: None,
                    role: None,
                    name: "deepseek-context".to_string(),
                    status: AgentStatus::Idle,
                    container_id: None,
                    docker_image: "ubuntu:latest".to_string(),
                    provider_id: "deepseek".to_string(),
                    provider_name: "DeepSeek".to_string(),
                    model: "deepseek-v4-pro".to_string(),
                    reasoning_effort: Some("high".to_string()),
                    created_at: timestamp,
                    updated_at: timestamp,
                    current_turn: None,
                    last_error: None,
                    token_usage: TokenUsage::default(),
                },
                None,
            )
            .await
            .expect("save agent");
        store
            .save_agent_session(
                agent_id,
                &AgentSessionSummary {
                    id: Uuid::new_v4(),
                    title: "Chat 1".to_string(),
                    created_at: timestamp,
                    updated_at: timestamp,
                    message_count: 0,
                },
            )
            .await
            .expect("save session");
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            store,
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");

        let detail = runtime.get_agent(agent_id, None).await.expect("detail");

        assert_eq!(
            detail
                .context_usage
                .as_ref()
                .map(|usage| usage.context_tokens),
            Some(1_000_000)
        );
        assert_eq!(
            detail.context_usage.as_ref().map(|usage| usage.used_tokens),
            Some(0)
        );
    }

    #[tokio::test]
    async fn agent_config_resolves_effective_default_and_validates_updates() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("open store"),
        );
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider(), alt_test_provider()],
                default_provider_id: Some("alt".to_string()),
            })
            .await
            .expect("save providers");
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");

        let config = runtime.agent_config().await.expect("config");
        assert_eq!(config.planner, None);
        assert_eq!(config.explorer, None);
        assert_eq!(config.executor, None);
        assert_eq!(config.reviewer, None);
        let effective = config.effective_executor.expect("effective default");
        assert_eq!(effective.provider_id, "alt");
        assert_eq!(effective.model, "alt-default");
        assert_eq!(effective.reasoning_effort, Some("medium".to_string()));
        assert_eq!(
            config.effective_planner.expect("planner default").model,
            "alt-default"
        );
        assert_eq!(
            config.effective_explorer.expect("explorer default").model,
            "alt-default"
        );
        assert_eq!(
            config.effective_reviewer.expect("reviewer default").model,
            "alt-default"
        );

        let updated = runtime
            .update_agent_config(AgentConfigRequest {
                executor: Some(AgentModelPreference {
                    provider_id: "openai".to_string(),
                    model: "gpt-5.4".to_string(),
                    reasoning_effort: Some("high".to_string()),
                }),
                ..Default::default()
            })
            .await
            .expect("update");
        assert_eq!(
            updated.effective_executor.expect("effective").model,
            "gpt-5.4"
        );

        let invalid = runtime
            .update_agent_config(AgentConfigRequest {
                reviewer: Some(AgentModelPreference {
                    provider_id: "openai".to_string(),
                    model: "gpt-5.4".to_string(),
                    reasoning_effort: Some("max".to_string()),
                }),
                ..Default::default()
            })
            .await;
        assert!(matches!(invalid, Err(RuntimeError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn spawn_agent_uses_executor_default_when_role_omitted() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("open store"),
        );
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider(), alt_test_provider()],
                default_provider_id: Some("alt".to_string()),
            })
            .await
            .expect("save providers");
        let parent_id = Uuid::new_v4();
        let timestamp = now();
        store
            .save_agent(
                &AgentSummary {
                    id: parent_id,
                    parent_id: None,
                    task_id: None,
                    role: None,
                    name: "parent".to_string(),
                    status: AgentStatus::Idle,
                    container_id: None,
                    docker_image: "ghcr.io/rcore-os/tgoskits-container:latest".to_string(),
                    provider_id: "openai".to_string(),
                    provider_name: "OpenAI".to_string(),
                    model: "gpt-5.4".to_string(),
                    reasoning_effort: Some("high".to_string()),
                    created_at: timestamp,
                    updated_at: timestamp,
                    current_turn: None,
                    last_error: None,
                    token_usage: TokenUsage::default(),
                },
                None,
            )
            .await
            .expect("save parent");
        store
            .save_agent_session(
                parent_id,
                &AgentSessionSummary {
                    id: Uuid::new_v4(),
                    title: "Chat 1".to_string(),
                    created_at: timestamp,
                    updated_at: timestamp,
                    message_count: 0,
                },
            )
            .await
            .expect("save session");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let parent = runtime.agent(parent_id).await.expect("parent");
        *parent.container.write().await = Some(ContainerHandle {
            id: "parent-container".to_string(),
            name: "parent-container".to_string(),
            image: "unused".to_string(),
        });

        let result = runtime
            .execute_tool_for_test(
                parent_id,
                "spawn_agent",
                json!({
                    "name": "child",
                    "provider_id": "openai",
                    "model": "gpt-5.4"
                }),
            )
            .await;
        assert!(result.expect("spawn agent").success);
        let child = runtime
            .list_agents()
            .await
            .into_iter()
            .find(|agent| agent.parent_id == Some(parent_id))
            .expect("child");
        assert_eq!(child.provider_id, "alt");
        assert_eq!(child.model, "alt-default");
        assert_eq!(child.reasoning_effort, Some("medium".to_string()));
        assert_eq!(
            child.docker_image,
            "ghcr.io/rcore-os/tgoskits-container:latest"
        );
        let docker_log = fake_docker_log(&dir);
        assert!(docker_log.contains("commit parent-container mai-team-snapshot-"));
        assert!(docker_log.contains(&format!("create --name mai-team-{}", child.id)));
        assert!(docker_log.contains("rmi -f mai-team-snapshot-"));
    }

    #[tokio::test]
    async fn spawn_agent_uses_role_config_over_parent_defaults() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("open store"),
        );
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider(), alt_test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        store
            .save_agent_config(&AgentConfigRequest {
                planner: Some(AgentModelPreference {
                    provider_id: "alt".to_string(),
                    model: "alt-default".to_string(),
                    reasoning_effort: Some("medium".to_string()),
                }),
                explorer: Some(AgentModelPreference {
                    provider_id: "openai".to_string(),
                    model: "gpt-5.5".to_string(),
                    reasoning_effort: Some("medium".to_string()),
                }),
                executor: Some(AgentModelPreference {
                    provider_id: "alt".to_string(),
                    model: "alt-research".to_string(),
                    reasoning_effort: Some("low".to_string()),
                }),
                reviewer: Some(AgentModelPreference {
                    provider_id: "openai".to_string(),
                    model: "gpt-5.4".to_string(),
                    reasoning_effort: Some("high".to_string()),
                }),
            })
            .await
            .expect("save config");
        let parent_id = Uuid::new_v4();
        let timestamp = now();
        store
            .save_agent(
                &AgentSummary {
                    id: parent_id,
                    parent_id: None,
                    task_id: None,
                    role: None,
                    name: "parent".to_string(),
                    status: AgentStatus::Idle,
                    container_id: None,
                    docker_image: "ghcr.io/rcore-os/tgoskits-container:latest".to_string(),
                    provider_id: "openai".to_string(),
                    provider_name: "OpenAI".to_string(),
                    model: "gpt-5.5".to_string(),
                    reasoning_effort: Some("high".to_string()),
                    created_at: timestamp,
                    updated_at: timestamp,
                    current_turn: None,
                    last_error: None,
                    token_usage: TokenUsage::default(),
                },
                None,
            )
            .await
            .expect("save parent");
        store
            .save_agent_session(
                parent_id,
                &AgentSessionSummary {
                    id: Uuid::new_v4(),
                    title: "Chat 1".to_string(),
                    created_at: timestamp,
                    updated_at: timestamp,
                    message_count: 0,
                },
            )
            .await
            .expect("save session");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let parent = runtime.agent(parent_id).await.expect("parent");
        *parent.container.write().await = Some(ContainerHandle {
            id: "parent-container".to_string(),
            name: "parent-container".to_string(),
            image: "unused".to_string(),
        });

        let result = runtime
            .execute_tool_for_test(
                parent_id,
                "spawn_agent",
                json!({
                    "name": "child",
                    "role": "reviewer",
                    "provider_id": "openai",
                    "model": "gpt-5.4"
                }),
            )
            .await;
        assert!(result.expect("spawn agent").success);
        let child = runtime
            .list_agents()
            .await
            .into_iter()
            .find(|agent| agent.parent_id == Some(parent_id))
            .expect("child");
        assert_eq!(child.provider_id, "openai");
        assert_eq!(child.model, "gpt-5.4");
        assert_eq!(child.reasoning_effort, Some("high".to_string()));
        assert_eq!(
            child.docker_image,
            "ghcr.io/rcore-os/tgoskits-container:latest"
        );
        let snapshot = store.load_runtime_snapshot(10).await.expect("snapshot");
        let child_record = snapshot
            .agents
            .into_iter()
            .find(|agent| agent.summary.id == child.id)
            .expect("child record");
        assert!(
            child_record
                .system_prompt
                .as_deref()
                .unwrap_or_default()
                .contains("Reviewer")
        );
    }

    #[tokio::test]
    async fn create_agent_persists_and_uses_explicit_docker_image() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("open store"),
        );
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let runtime = AgentRuntime::new(
            DockerClient::new_with_binary("ubuntu:latest", fake_docker_path(&dir)),
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");

        let image = "ghcr.io/rcore-os/tgoskits-container:latest";
        let agent = runtime
            .create_agent(CreateAgentRequest {
                name: Some("custom-image".to_string()),
                provider_id: Some("openai".to_string()),
                model: Some("gpt-5.5".to_string()),
                reasoning_effort: None,
                docker_image: Some(format!("  {image}  ")),
                parent_id: None,
                system_prompt: None,
            })
            .await
            .expect("create agent");

        assert_eq!(agent.docker_image, image);
        assert!(fake_docker_log(&dir).contains(image));
        let snapshot = store.load_runtime_snapshot(10).await.expect("snapshot");
        assert_eq!(snapshot.agents[0].summary.docker_image, image);
    }

    #[tokio::test]
    async fn update_agent_changes_model_persists_and_publishes() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("open store"),
        );
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let agent_id = Uuid::new_v4();
        let timestamp = now();
        let summary = AgentSummary {
            id: agent_id,
            parent_id: None,
            task_id: None,
            role: None,
            name: "model-switch".to_string(),
            status: AgentStatus::Idle,
            container_id: None,
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "openai".to_string(),
            provider_name: "OpenAI".to_string(),
            model: "gpt-5.5".to_string(),
            reasoning_effort: Some("low".to_string()),
            created_at: timestamp,
            updated_at: timestamp,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        };
        store.save_agent(&summary, None).await.expect("save agent");
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");
        let mut events = runtime.subscribe();

        let updated = runtime
            .update_agent(
                agent_id,
                UpdateAgentRequest {
                    provider_id: None,
                    model: Some("gpt-5.4".to_string()),
                    reasoning_effort: Some("high".to_string()),
                },
            )
            .await
            .expect("update");

        assert_eq!(updated.model, "gpt-5.4");
        assert_eq!(updated.reasoning_effort, Some("high".to_string()));
        let event = events.recv().await.expect("event");
        assert!(matches!(
            event.kind,
            ServiceEventKind::AgentUpdated { agent } if agent.id == agent_id
                && agent.model == "gpt-5.4"
                && agent.reasoning_effort == Some("high".to_string())
        ));
        let snapshot = store.load_runtime_snapshot(10).await.expect("snapshot");
        assert_eq!(snapshot.agents[0].summary.model, "gpt-5.4");
        assert_eq!(
            snapshot.agents[0].summary.reasoning_effort,
            Some("high".to_string())
        );
    }

    #[tokio::test]
    async fn update_agent_rejects_invalid_reasoning_and_clears_unsupported_model() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("open store"),
        );
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let agent_id = Uuid::new_v4();
        let timestamp = now();
        let summary = AgentSummary {
            id: agent_id,
            parent_id: None,
            task_id: None,
            role: None,
            name: "reasoning-switch".to_string(),
            status: AgentStatus::Idle,
            container_id: None,
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "openai".to_string(),
            provider_name: "OpenAI".to_string(),
            model: "gpt-5.5".to_string(),
            reasoning_effort: Some("medium".to_string()),
            created_at: timestamp,
            updated_at: timestamp,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        };
        store.save_agent(&summary, None).await.expect("save agent");
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");

        let invalid = runtime
            .update_agent(
                agent_id,
                UpdateAgentRequest {
                    provider_id: None,
                    model: Some("gpt-5.4".to_string()),
                    reasoning_effort: Some("max".to_string()),
                },
            )
            .await;
        assert!(matches!(invalid, Err(RuntimeError::InvalidInput(_))));

        let updated = runtime
            .update_agent(
                agent_id,
                UpdateAgentRequest {
                    provider_id: None,
                    model: Some("gpt-4.1".to_string()),
                    reasoning_effort: Some("high".to_string()),
                },
            )
            .await
            .expect("clear unsupported");
        assert_eq!(updated.model, "gpt-4.1");
        assert_eq!(updated.reasoning_effort, None);
    }

    #[tokio::test]
    async fn update_agent_rejects_busy_and_unknown_model() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("open store"),
        );
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let agent_id = Uuid::new_v4();
        let timestamp = now();
        let summary = AgentSummary {
            id: agent_id,
            parent_id: None,
            task_id: None,
            role: None,
            name: "busy".to_string(),
            status: AgentStatus::Idle,
            container_id: None,
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "openai".to_string(),
            provider_name: "OpenAI".to_string(),
            model: "gpt-5.5".to_string(),
            reasoning_effort: Some("medium".to_string()),
            created_at: timestamp,
            updated_at: timestamp,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        };
        store.save_agent(&summary, None).await.expect("save agent");
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            store,
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");

        let unknown = runtime
            .update_agent(
                agent_id,
                UpdateAgentRequest {
                    provider_id: None,
                    model: Some("missing".to_string()),
                    reasoning_effort: None,
                },
            )
            .await;
        assert!(matches!(unknown, Err(RuntimeError::Store(_))));

        let agent = runtime.agent(agent_id).await.expect("agent");
        {
            let mut summary = agent.summary.write().await;
            summary.status = AgentStatus::RunningTurn;
            summary.current_turn = Some(Uuid::new_v4());
        }
        let busy = runtime
            .update_agent(
                agent_id,
                UpdateAgentRequest {
                    provider_id: None,
                    model: Some("gpt-5.4".to_string()),
                    reasoning_effort: None,
                },
            )
            .await;
        assert!(matches!(busy, Err(RuntimeError::AgentBusy(id)) if id == agent_id));
    }

    #[test]
    fn tool_event_preview_redacts_sensitive_and_large_values() {
        let value = json!({
            "command": "echo ok",
            "api_key": "secret",
            "content_base64": "a".repeat(320),
        });

        let preview = trace_preview_value(&value, 1_000);

        assert!(preview.contains("echo ok"));
        assert!(preview.contains("<redacted>"));
        assert!(!preview.contains("secret"));
        assert!(!preview.contains(&"a".repeat(120)));
    }
}
