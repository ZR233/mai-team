use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, TimeDelta, Utc};
use futures::future::{AbortHandle, Abortable};
use mai_agents::AgentProfilesManager;
use mai_docker::{
    ContainerCreateOptions, ContainerHandle, DockerClient, project_review_workspace_volume,
    project_workspace_volume,
};
use mai_mcp::McpAgentManager;
#[cfg(test)]
use mai_mcp::McpTool;
use mai_model::{ModelClient, ModelTurnState};
use mai_protocol::*;
#[cfg(test)]
use mai_protocol::{MessageRole, ModelContentItem, ModelToolCall};
use mai_skills::{SkillInjections, SkillsManager};
use mai_store::{AgentLogFilter, ConfigStore, ProviderSelection, ToolTraceFilter};
#[cfg(test)]
use mai_tools::build_tool_definitions_with_filter;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio::time::{Duration, Instant, sleep};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod agents;
mod deps;
mod events;
mod github;
mod instructions;
mod projects;
mod state;
mod tools;
mod turn;

use deps::RuntimeDeps;
use events::{RECENT_EVENT_LIMIT, RuntimeEvents};
use github::{
    DEFAULT_GITHUB_API_BASE_URL, DirectGithubAppBackend, GITHUB_HTTP_TIMEOUT_SECS,
    GithubAppBackend, GithubErrorResponse, github_api_url, github_clone_url, github_headers,
    normalize_github_api_get_path, repository_packages_with_token,
};
use instructions::{CONTAINER_SKILLS_ROOT, ContainerSkillPaths};
use projects::mcp::PROJECT_WORKSPACE_PATH;
use projects::review::ProjectReviewCycleResult;
use projects::review::runs::FinishReviewRun;
#[cfg(test)]
use projects::skills::PROJECT_SKILLS_CACHE_DIR;
use projects::skills::{ProjectSkillRefreshSource, ProjectSkillSourceDir};
use state::{
    AgentRecord, AgentSessionRecord, ProjectRecord, ProjectReviewWorker, RuntimeState, TaskRecord,
    TurnControl, TurnGuard,
};
use turn::completion::TurnResult;
use turn::tools::ToolExecution;

const AUTO_COMPACT_THRESHOLD_PERCENT: u64 = 90;
const REVIEW_ROUND_LIMIT: u64 = 5;
const PROJECT_REVIEW_FAILURE_RETRY_SECS: u64 = 600;
const PROJECT_REVIEW_HISTORY_RETENTION_DAYS: i64 = 5;
const PROJECT_REVIEW_CLEANUP_INTERVAL_SECS: u64 = 3600;
const PROJECT_REVIEW_RUN_LIST_LIMIT: usize = 50;
const PROJECT_REVIEW_SNAPSHOT_MESSAGE_LIMIT: usize = 40;
const PROJECT_REVIEW_SNAPSHOT_EVENT_LIMIT: usize = 80;
const SKILL_RESOURCE_SERVER: &str = "skill";
const PROJECT_SKILL_RESOURCE_SERVER: &str = "project-skill";
const SKILL_RESOURCE_SCHEME: &str = "skill:///";
const DEFAULT_SIDECAR_IMAGE: &str = "ghcr.io/zr233/mai-team-sidecar:latest";
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
const TURN_CANCEL_GRACE: Duration = Duration::from_millis(500);
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
    #[error("project not found: {0}")]
    ProjectNotFound(ProjectId),
    #[error("project review run not found: {0}")]
    ProjectReviewRunNotFound(Uuid),
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
    #[error("turn not found: {agent_id}/{turn_id}")]
    TurnNotFound { agent_id: AgentId, turn_id: TurnId },
    #[error("turn cancelled")]
    TurnCancelled,
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
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("jwt error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

#[derive(Clone)]
pub struct RuntimeConfig {
    pub repo_root: PathBuf,
    pub cache_root: PathBuf,
    pub artifact_files_root: PathBuf,
    pub sidecar_image: String,
    pub github_api_base_url: Option<String>,
    pub git_binary: Option<String>,
    pub system_skills_root: Option<PathBuf>,
    pub system_agents_root: Option<PathBuf>,
}

#[derive(Default)]
struct ReviewStateUpdate {
    current_reviewer_agent_id: Option<AgentId>,
    next_review_at: Option<DateTime<Utc>>,
    outcome: Option<ProjectReviewOutcome>,
    #[allow(dead_code)]
    summary_text: Option<String>,
    error: Option<String>,
    force_disabled: bool,
}

pub struct AgentRuntime {
    deps: RuntimeDeps,
    state: RuntimeState,
    events: RuntimeEvents,
    cache_root: PathBuf,
    artifact_files_root: PathBuf,
    sidecar_image: String,
    github_api_base_url: String,
}

#[derive(Debug, Clone)]
enum ContainerSource {
    FreshImage,
    ImageWithWorkspace {
        workspace_volume: String,
    },
    CloneFrom {
        parent_container_id: String,
        docker_image: String,
        workspace_volume: Option<String>,
    },
}

struct ResolvedAgentModel {
    preference: AgentModelPreference,
    effective: ResolvedAgentModelPreference,
}

struct AgentResourceBroker {
    agent_mcp: Option<Arc<McpAgentManager>>,
    project_mcp: Option<Arc<McpAgentManager>>,
    skills: SkillsListResponse,
    _project_skill_guard: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
}

impl AgentRuntime {
    pub async fn new(
        docker: DockerClient,
        model: ModelClient,
        store: Arc<ConfigStore>,
        config: RuntimeConfig,
    ) -> Result<Arc<Self>> {
        Self::new_with_github_backend(docker, model, store, config, None).await
    }

    pub async fn new_with_github_backend(
        docker: DockerClient,
        model: ModelClient,
        store: Arc<ConfigStore>,
        config: RuntimeConfig,
        github_backend: Option<Arc<dyn GithubAppBackend>>,
    ) -> Result<Arc<Self>> {
        let skills = SkillsManager::new_with_system_root(
            &config.repo_root,
            config.system_skills_root.as_ref(),
        );
        let agent_profiles = AgentProfilesManager::new_with_system_root(
            &config.repo_root,
            config.system_agents_root.as_ref(),
        );
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
                sessions.push(agents::default_session_record());
            }
            let agent = Arc::new(AgentRecord {
                summary: RwLock::new(summary.clone()),
                sessions: Mutex::new(sessions),
                container: RwLock::new(None),
                mcp: RwLock::new(None),
                system_prompt: persisted.system_prompt,
                turn_lock: Mutex::new(()),
                cancel_requested: AtomicBool::new(false),
                active_turn: StdMutex::new(None),
                pending_inputs: Mutex::new(VecDeque::new()),
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
        let mut projects = HashMap::new();
        for mut summary in snapshot.projects {
            let project_id = summary.id;
            let project_agents = agents
                .values()
                .filter(|agent| {
                    agent
                        .summary
                        .try_read()
                        .ok()
                        .and_then(|summary| summary.project_id)
                        == Some(project_id)
                })
                .count();
            if project_agents == 0 {
                summary.status = ProjectStatus::Failed;
                summary.clone_status = ProjectCloneStatus::Failed;
                summary.last_error = Some("maintainer agent is missing".to_string());
                store.save_project(&summary).await?;
            }
            projects.insert(
                summary.id,
                Arc::new(ProjectRecord {
                    summary: RwLock::new(summary),
                    sidecar: RwLock::new(None),
                    review_worker: Mutex::new(None),
                }),
            );
        }
        let sidecar_image = runtime_sidecar_image(config.sidecar_image);
        let github_api_base_url = config
            .github_api_base_url
            .as_deref()
            .unwrap_or(DEFAULT_GITHUB_API_BASE_URL)
            .to_string();
        let github_http = reqwest::Client::builder()
            .timeout(Duration::from_secs(GITHUB_HTTP_TIMEOUT_SECS))
            .build()?;
        let github_backend = github_backend.unwrap_or_else(|| {
            Arc::new(DirectGithubAppBackend::new(
                Arc::clone(&store),
                github_http.clone(),
                github_api_base_url.clone(),
            ))
        });
        let git_accounts = Arc::new(github::GitAccountService::new(
            Arc::clone(&store),
            github_http.clone(),
            github_api_base_url.clone(),
            Arc::clone(&github_backend),
        ));

        let runtime = Arc::new(Self {
            deps: RuntimeDeps {
                docker,
                model,
                store: Arc::clone(&store),
                skills,
                agent_profiles,
                github_http,
                github_backend,
                git_accounts,
            },
            state: RuntimeState::new(agents, tasks, projects),
            events: RuntimeEvents::new(
                Arc::clone(&store),
                snapshot.next_sequence,
                snapshot.recent_events,
            ),
            cache_root: config.cache_root,
            artifact_files_root: config.artifact_files_root,
            sidecar_image,
            github_api_base_url,
        });
        let cleanup_runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            cleanup_runtime.run_project_review_cleanup_loop().await;
        });
        runtime.reconcile_project_review_singletons().await;
        runtime.start_enabled_project_review_workers().await;
        Ok(runtime)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        self.events.subscribe()
    }

    pub async fn agent_config(&self) -> Result<AgentConfigResponse> {
        let config = self.deps.store.load_agent_config().await?;
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

    pub async fn list_skills(&self) -> Result<SkillsListResponse> {
        let config = self.deps.store.load_skills_config().await?;
        Ok(self.deps.skills.list(&config)?)
    }

    pub async fn list_agent_profiles(&self) -> Result<AgentProfilesResponse> {
        Ok(self.deps.agent_profiles.list())
    }

    pub async fn update_skills_config(
        &self,
        request: SkillsConfigRequest,
    ) -> Result<SkillsListResponse> {
        let normalized = mai_skills::normalize_config(&request)?;
        self.deps.store.save_skills_config(&normalized).await?;
        Ok(self.deps.skills.list(&normalized)?)
    }

    pub async fn list_project_skills(&self, project_id: ProjectId) -> Result<SkillsListResponse> {
        if !self.project_skill_cache_dir(project_id).exists() {
            return self.detect_project_skills(project_id).await;
        }
        self.project_skills_from_cache(project_id).await
    }

    pub async fn detect_project_skills(&self, project_id: ProjectId) -> Result<SkillsListResponse> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        if summary.status != ProjectStatus::Ready
            || summary.clone_status != ProjectCloneStatus::Ready
        {
            return Err(RuntimeError::InvalidInput(
                "project repository workspace is not ready".to_string(),
            ));
        }

        let sidecar = self.ensure_project_sidecar(project_id).await?;
        let existing = self.existing_project_skill_dirs(&sidecar.id).await?;
        self.refresh_project_skill_cache(
            project_id,
            ProjectSkillRefreshSource::ProjectSidecar,
            Some(&sidecar.id),
            &existing,
        )
        .await?;
        self.project_skills_from_cache(project_id).await
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
        self.deps.store.save_agent_config(&request).await?;
        self.agent_config().await
    }

    pub async fn list_tasks(&self) -> Vec<TaskSummary> {
        let task_records = {
            let tasks = self.state.tasks.read().await;
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

    pub async fn list_projects(&self) -> Vec<ProjectSummary> {
        projects::service::list_projects(&self.state).await
    }

    pub async fn list_git_accounts(&self) -> Result<GitAccountsResponse> {
        self.deps.git_accounts.list().await
    }

    pub async fn save_git_account(
        self: &Arc<Self>,
        request: GitAccountRequest,
    ) -> Result<GitAccountResponse> {
        self.deps.git_accounts.save(request).await
    }

    pub async fn verify_git_account(&self, account_id: &str) -> Result<GitAccountSummary> {
        self.deps.git_accounts.verify(account_id).await
    }

    pub async fn delete_git_account(&self, account_id: &str) -> Result<GitAccountsResponse> {
        self.deps.git_accounts.delete(account_id).await
    }

    pub async fn set_default_git_account(&self, account_id: &str) -> Result<GitAccountsResponse> {
        self.deps.git_accounts.set_default(account_id).await
    }

    pub async fn list_git_account_repositories(
        &self,
        account_id: &str,
    ) -> Result<GithubRepositoriesResponse> {
        self.deps.git_accounts.list_repositories(account_id).await
    }

    pub fn runtime_defaults(&self) -> RuntimeDefaultsResponse {
        RuntimeDefaultsResponse {
            default_docker_image: self.deps.docker.image().to_string(),
        }
    }

    pub async fn list_git_account_repository_packages(
        &self,
        account_id: &str,
        owner: &str,
        repo: &str,
    ) -> Result<RepositoryPackagesResponse> {
        self.deps
            .git_accounts
            .list_repository_packages(account_id, owner, repo)
            .await
    }

    pub async fn list_github_installation_repository_packages(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
    ) -> Result<RepositoryPackagesResponse> {
        let token = self
            .deps
            .github_backend
            .github_installation_token(installation_id, None, true)
            .await?
            .token;
        repository_packages_with_token(
            &self.deps.github_http,
            &self.github_api_base_url,
            &token,
            owner,
            repo,
        )
        .await
    }

    pub async fn github_app_settings(&self) -> Result<GithubAppSettingsResponse> {
        self.deps.github_backend.github_app_settings().await
    }

    pub async fn save_github_app_settings(
        &self,
        request: GithubAppSettingsRequest,
    ) -> Result<GithubAppSettingsResponse> {
        self.deps
            .github_backend
            .save_github_app_settings(request)
            .await
    }

    pub async fn start_github_app_manifest(
        &self,
        request: GithubAppManifestStartRequest,
    ) -> Result<GithubAppManifestStartResponse> {
        self.deps
            .github_backend
            .start_github_app_manifest(request)
            .await
    }

    pub async fn complete_github_app_manifest(
        &self,
        code: &str,
        state: &str,
    ) -> Result<GithubAppSettingsResponse> {
        self.deps
            .github_backend
            .complete_github_app_manifest(code, state)
            .await
    }

    pub async fn list_github_installations(&self) -> Result<GithubInstallationsResponse> {
        self.deps.github_backend.list_github_installations().await
    }

    pub async fn refresh_github_installations(&self) -> Result<GithubInstallationsResponse> {
        self.deps
            .github_backend
            .refresh_github_installations()
            .await
    }

    pub async fn list_github_repositories(
        &self,
        installation_id: u64,
    ) -> Result<GithubRepositoriesResponse> {
        self.deps
            .github_backend
            .list_github_repositories(installation_id)
            .await
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
        let user_omitted_title = title.as_ref().map(|v| v.trim().is_empty()).unwrap_or(true);
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
                    system_prompt: Some(
                        agents::task_role_system_prompt(AgentRole::Planner).to_string(),
                    ),
                },
                ContainerSource::FreshImage,
                Some(task_id),
                None,
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
        self.deps.store.save_task(&summary, &plan).await?;
        let task = Arc::new(TaskRecord {
            summary: RwLock::new(summary.clone()),
            plan: RwLock::new(plan),
            plan_history: RwLock::new(Vec::new()),
            reviews: RwLock::new(Vec::new()),
            artifacts: RwLock::new(Vec::new()),
            workflow_lock: Mutex::new(()),
        });
        self.state.tasks.write().await.insert(task_id, task);
        self.events
            .publish(ServiceEventKind::TaskCreated {
                task: summary.clone(),
            })
            .await;
        let message_for_title = initial_message
            .as_ref()
            .filter(|m| !m.trim().is_empty())
            .cloned();
        if let Some(message) = initial_message.filter(|message| !message.trim().is_empty()) {
            let _ = self.send_task_message(task_id, message, Vec::new()).await?;
        }
        if user_omitted_title && let Some(message_text) = message_for_title {
            let runtime = Arc::clone(self);
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
        Ok(summary)
    }

    async fn generate_task_title(self: &Arc<Self>, message: &str) -> Result<String> {
        let planner_model = self.resolve_role_agent_model(AgentRole::Planner).await?;
        let selection = self
            .deps
            .store
            .resolve_provider(
                Some(&planner_model.preference.provider_id),
                Some(&planner_model.preference.model),
            )
            .await?;
        let instructions = "Generate a concise task title of 3-8 words that captures the essence of the user's request. Output only the title text, nothing else. Do not use quotes or punctuation at the end.";
        let input = vec![ModelInputItem::user_text(message)];
        let resolved = self
            .deps
            .model
            .resolve(&selection.provider, &selection.model, None);
        let response = turn::model_stream::consume_model_stream_to_response(
            &self.deps.model,
            &resolved,
            instructions,
            &input,
            &[],
            &mut ModelTurnState::default(),
            &CancellationToken::new(),
        )
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
            self.deps.store.save_task(&summary, &plan).await?;
            self.events
                .publish(ServiceEventKind::TaskUpdated {
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

    pub async fn get_project(
        &self,
        project_id: ProjectId,
        selected_agent_id: Option<AgentId>,
        session_id: Option<SessionId>,
    ) -> Result<ProjectDetail> {
        projects::service::get_project(&self.state, self, project_id, selected_agent_id, session_id)
            .await
    }

    pub async fn list_project_review_runs(
        &self,
        project_id: ProjectId,
        offset: usize,
        limit: usize,
    ) -> Result<ProjectReviewRunsResponse> {
        self.project(project_id).await?;
        projects::review::runs::list_project_review_runs(
            &self.deps.store,
            project_id,
            PROJECT_REVIEW_HISTORY_RETENTION_DAYS,
            offset,
            limit,
        )
        .await
    }

    pub async fn get_project_review_run(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
    ) -> Result<ProjectReviewRunDetail> {
        self.project(project_id).await?;
        projects::review::runs::get_project_review_run(&self.deps.store, project_id, run_id).await
    }

    pub async fn create_project(
        self: &Arc<Self>,
        request: CreateProjectRequest,
    ) -> Result<ProjectSummary> {
        let relay_installation_id = request.installation_id;
        let account_id = match request
            .git_account_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
        {
            Some(account_id) => account_id,
            None if relay_installation_id > 0 => {
                let installations = self.list_github_installations().await?;
                let installation = installations
                    .installations
                    .into_iter()
                    .find(|installation| installation.id == relay_installation_id)
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput("GitHub App installation not found".to_string())
                    })?;
                self.deps
                    .store
                    .upsert_github_app_relay_account(
                        relay_installation_id,
                        &installation.account_login,
                        "default",
                        false,
                    )
                    .await?
                    .id
            }
            None => {
                return Err(RuntimeError::InvalidInput(
                    "git_account_id or installation_id is required".to_string(),
                ));
            }
        };
        let repository_ref = request
            .repository_full_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let owner = request.owner.trim();
                let repo = request.repo.trim();
                (!owner.is_empty() && !repo.is_empty()).then(|| format!("{owner}/{repo}"))
            })
            .ok_or_else(|| {
                RuntimeError::InvalidInput("repository_full_name is required".to_string())
            })?;
        let repository = self
            .deps
            .git_accounts
            .verified_repository(&account_id, &repository_ref)
            .await?;
        let owner = repository.owner.clone();
        let repo = repository.name.clone();
        let repository_id = repository.id;
        let branch = normalize_optional_path_segment(request.branch.as_deref(), "branch")?
            .unwrap_or_else(|| repository.default_branch.clone());
        let name = request.name.trim().to_string();
        let name = if name.is_empty() {
            format!("{owner}/{repo}")
        } else {
            name
        };
        let account = self.deps.git_accounts.summary(&account_id).await?;
        let installation_id = account.installation_id.unwrap_or(relay_installation_id);
        let installation_account = account
            .installation_account
            .clone()
            .or(account.login)
            .unwrap_or(account.label);
        let project_id = Uuid::new_v4();
        let planner_model = self.resolve_role_agent_model(AgentRole::Planner).await?;
        let clone_url = github_clone_url(&owner, &repo);
        let system_prompt = project_maintainer_system_prompt(&owner, &repo, &clone_url, &branch);
        let maintainer = self
            .create_agent_record(
                CreateAgentRequest {
                    name: Some(format!("{name} Maintainer")),
                    provider_id: Some(planner_model.preference.provider_id),
                    model: Some(planner_model.preference.model),
                    reasoning_effort: planner_model.preference.reasoning_effort,
                    docker_image: request.docker_image.clone(),
                    parent_id: None,
                    system_prompt: Some(system_prompt),
                },
                None,
                Some(project_id),
                Some(AgentRole::Planner),
            )
            .await?;
        let maintainer_summary = maintainer.summary.read().await.clone();
        let created_at = now();
        let project = ProjectSummary {
            id: project_id,
            name,
            status: ProjectStatus::Creating,
            owner,
            repo,
            repository_full_name: repository.full_name,
            git_account_id: Some(account_id),
            repository_id,
            installation_id,
            installation_account,
            branch,
            docker_image: maintainer_summary.docker_image.clone(),
            clone_status: ProjectCloneStatus::Pending,
            maintainer_agent_id: maintainer_summary.id,
            created_at,
            updated_at: created_at,
            last_error: None,
            auto_review_enabled: request.auto_review_enabled,
            reviewer_extra_prompt: normalize_optional_text(request.reviewer_extra_prompt),
            review_status: if request.auto_review_enabled {
                ProjectReviewStatus::Idle
            } else {
                ProjectReviewStatus::Disabled
            },
            current_reviewer_agent_id: None,
            last_review_started_at: None,
            last_review_finished_at: None,
            next_review_at: None,
            last_review_outcome: None,
            review_last_error: None,
        };
        self.deps.store.save_project(&project).await?;
        self.state.projects.write().await.insert(
            project_id,
            Arc::new(ProjectRecord {
                summary: RwLock::new(project.clone()),
                sidecar: RwLock::new(None),
                review_worker: Mutex::new(None),
            }),
        );
        self.events
            .publish(ServiceEventKind::ProjectCreated {
                project: project.clone(),
            })
            .await;
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = runtime
                .start_project_workspace(project_id, maintainer_summary.id)
                .await
            {
                tracing::warn!(project_id = %project_id, "failed to finish project workspace setup: {err}");
            }
        });
        Ok(project)
    }

    pub async fn update_project(
        self: &Arc<Self>,
        project_id: ProjectId,
        request: UpdateProjectRequest,
    ) -> Result<ProjectSummary> {
        let project = self.project(project_id).await?;
        let updated = {
            let mut summary = project.summary.write().await;
            if let Some(name) = request.name {
                let name = name.trim();
                if !name.is_empty() {
                    summary.name = name.to_string();
                }
            }
            if let Some(docker_image) = request.docker_image {
                let docker_image = docker_image.trim();
                if !docker_image.is_empty() {
                    summary.docker_image = docker_image.to_string();
                }
            }
            if let Some(enabled) = request.auto_review_enabled {
                summary.auto_review_enabled = enabled;
                if enabled && summary.review_status == ProjectReviewStatus::Disabled {
                    summary.review_status = ProjectReviewStatus::Idle;
                }
                if !enabled {
                    summary.review_status = ProjectReviewStatus::Disabled;
                    summary.current_reviewer_agent_id = None;
                    summary.next_review_at = None;
                }
            }
            if request.reviewer_extra_prompt.is_some() {
                summary.reviewer_extra_prompt =
                    normalize_optional_text(request.reviewer_extra_prompt);
            }
            summary.updated_at = now();
            summary.clone()
        };
        self.deps.store.save_project(&updated).await?;
        self.events
            .publish(ServiceEventKind::ProjectUpdated {
                project: updated.clone(),
            })
            .await;
        if updated.auto_review_enabled {
            self.start_project_review_loop_if_ready(project_id).await?;
        } else {
            self.stop_project_review_loop(project_id).await;
        }
        Ok(updated)
    }

    pub async fn delete_project(self: &Arc<Self>, project_id: ProjectId) -> Result<()> {
        let project = self.project(project_id).await?;
        self.stop_project_review_loop(project_id).await;
        let root_agents = self
            .project_agents(project_id)
            .await
            .into_iter()
            .filter(|agent| agent.parent_id.is_none())
            .map(|agent| agent.id)
            .collect::<Vec<_>>();
        {
            let mut summary = project.summary.write().await;
            summary.status = ProjectStatus::Deleting;
            summary.updated_at = now();
            self.deps.store.save_project(&summary).await?;
            self.events
                .publish(ServiceEventKind::ProjectUpdated {
                    project: summary.clone(),
                })
                .await;
        }
        for agent_id in root_agents {
            let _ = self.delete_agent(agent_id).await;
        }
        self.shutdown_project_mcp_manager(project_id).await;
        let _ = self.delete_project_sidecar(project_id).await;
        let _ = self.delete_project_review_workspace(project_id).await;
        self.deps.store.delete_project(project_id).await?;
        self.state.projects.write().await.remove(&project_id);
        self.state
            .project_skill_locks
            .write()
            .await
            .remove(&project_id);
        self.events
            .publish(ServiceEventKind::ProjectDeleted { project_id })
            .await;
        let _ = fs::remove_dir_all(self.project_skill_cache_dir(project_id));
        Ok(())
    }

    pub async fn cancel_project(self: &Arc<Self>, project_id: ProjectId) -> Result<()> {
        let project = self.project(project_id).await?;
        self.stop_project_review_loop(project_id).await;
        let agents = self.project_agents(project_id).await;
        for agent in agents {
            if let Ok(record) = self.agent(agent.id).await {
                let current_turn = record.summary.read().await.current_turn;
                if let Some(turn_id) = current_turn {
                    let _ = self.cancel_agent_turn(agent.id, turn_id).await;
                } else {
                    record.cancel_requested.store(true, Ordering::SeqCst);
                    let _ = self.set_status(&record, AgentStatus::Cancelled, None).await;
                }
            }
        }
        let updated = {
            let mut summary = project.summary.write().await;
            if matches!(summary.status, ProjectStatus::Creating) {
                summary.status = ProjectStatus::Failed;
                summary.last_error = Some("cancelled".to_string());
            }
            summary.updated_at = now();
            summary.clone()
        };
        self.deps.store.save_project(&updated).await?;
        self.events
            .publish(ServiceEventKind::ProjectUpdated { project: updated })
            .await;
        self.shutdown_project_mcp_manager(project_id).await;
        let _ = self.delete_project_sidecar(project_id).await;
        Ok(())
    }

    pub async fn send_project_message(
        self: &Arc<Self>,
        project_id: ProjectId,
        request: SendMessageRequest,
    ) -> Result<TurnId> {
        let project = self.project(project_id).await?;
        let maintainer_agent_id = project.summary.read().await.maintainer_agent_id;
        self.send_message(
            maintainer_agent_id,
            request.session_id,
            request.message,
            request.skill_mentions,
        )
        .await
    }

    pub async fn publish_external_event(&self, kind: ServiceEventKind) {
        self.events.publish(kind).await;
    }

    pub async fn find_project_for_github_event(
        &self,
        installation_id: Option<u64>,
        repository_id: Option<u64>,
        repository_full_name: Option<&str>,
    ) -> Option<ProjectId> {
        projects::service::find_project_for_github_event(
            &self.state,
            installation_id,
            repository_id,
            repository_full_name,
        )
        .await
    }

    pub async fn handle_project_push_event(
        self: &Arc<Self>,
        project_id: ProjectId,
        payload: &Value,
    ) -> Result<()> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let pushed_ref = payload
            .get("ref")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let default_ref = format!("refs/heads/{}", summary.branch);
        if pushed_ref == default_ref
            && summary.status == ProjectStatus::Ready
            && summary.clone_status == ProjectCloneStatus::Ready
        {
            self.shutdown_project_mcp_manager(project_id).await;
            self.sync_project_review_repo(project_id).await?;
            let _ = self
                .refresh_project_skills_from_review_workspace(project_id)
                .await;
            self.events
                .publish(ServiceEventKind::ProjectUpdated { project: summary })
                .await;
        }
        Ok(())
    }

    pub async fn trigger_project_review(
        self: &Arc<Self>,
        project_id: ProjectId,
        pr: u64,
        delivery_id: String,
        reason: String,
    ) -> Result<()> {
        let project = self.project(project_id).await?;
        {
            let summary = project.summary.read().await;
            if !summary.auto_review_enabled {
                return Ok(());
            }
            if summary.status != ProjectStatus::Ready
                || summary.clone_status != ProjectCloneStatus::Ready
            {
                return Ok(());
            }
            if matches!(
                summary.review_status,
                ProjectReviewStatus::Syncing | ProjectReviewStatus::Running
            ) {
                tracing::info!(
                    project_id = %project_id,
                    pr,
                    delivery_id = %delivery_id,
                    "project review already active; webhook review trigger recorded only"
                );
                return Ok(());
            }
        }
        {
            let mut worker = project.review_worker.lock().await;
            if let Some(worker) = worker.take() {
                worker.cancellation_token.cancel();
                worker.abort_handle.abort();
            }
        }
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            let cancellation_token = CancellationToken::new();
            if let Err(err) = runtime.ensure_project_review_workspace(project_id).await {
                let _ = projects::review::runs::record_project_review_startup_failure(
                    &runtime.deps.store,
                    project_id,
                    err.to_string(),
                )
                .await;
                return;
            }
            let result = runtime
                .run_project_review_once(project_id, cancellation_token, Some(pr))
                .await;
            let decision = match result {
                Ok(result) => projects::review::project_review_loop_decision_for_result(result),
                Err(err) => {
                    projects::review::project_review_loop_decision_for_error(err.to_string())
                }
            };
            let next_review_at = (decision.delay.as_secs() > 0)
                .then(|| Utc::now() + TimeDelta::seconds(decision.delay.as_secs() as i64));
            let _ = runtime
                .set_project_review_state(
                    project_id,
                    decision.status,
                    ReviewStateUpdate {
                        next_review_at,
                        outcome: decision.outcome,
                        summary_text: decision.summary,
                        error: decision.error,
                        ..Default::default()
                    },
                )
                .await;
            if let Err(err) = runtime.start_project_review_loop_if_ready(project_id).await {
                tracing::warn!(project_id = %project_id, "failed to resume review loop after webhook trigger: {err}");
            }
        });
        tracing::info!(
            project_id = %project_id,
            pr,
            delivery_id = %delivery_id,
            reason = %reason,
            "queued targeted project review"
        );
        Ok(())
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
                self.deps
                    .store
                    .save_plan_history_entry(task_id, &entry)
                    .await?;
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
                self.deps.store.save_task(&summary, &plan).await?;
                self.events
                    .publish(ServiceEventKind::PlanUpdated {
                        task_id,
                        plan: plan.clone(),
                    })
                    .await;
                self.events
                    .publish(ServiceEventKind::TaskUpdated {
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
            self.deps.store.save_task(&summary, &plan).await?;
            self.events
                .publish(ServiceEventKind::TaskUpdated {
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
            self.deps
                .store
                .save_plan_history_entry(task_id, &entry)
                .await?;
            task.plan_history.write().await.push(entry);
            plan.status = PlanStatus::NeedsRevision;
            plan.revision_feedback = Some(feedback.clone());
            plan.revision_requested_at = Some(now());
            let mut summary = task.summary.write().await;
            summary.status = TaskStatus::Planning;
            summary.plan_status = PlanStatus::NeedsRevision;
            summary.updated_at = now();
            self.deps.store.save_task(&summary, &plan).await?;
            self.events
                .publish(ServiceEventKind::PlanUpdated {
                    task_id,
                    plan: plan.clone(),
                })
                .await;
            self.events
                .publish(ServiceEventKind::TaskUpdated {
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
        self.create_agent_with_container_source(
            request,
            ContainerSource::FreshImage,
            None,
            None,
            None,
        )
        .await
    }

    async fn create_agent_with_container_source(
        self: &Arc<Self>,
        request: CreateAgentRequest,
        container_source: ContainerSource,
        task_id: Option<TaskId>,
        project_id: Option<ProjectId>,
        role: Option<AgentRole>,
    ) -> Result<AgentSummary> {
        let agent = self
            .create_agent_record(request, task_id, project_id, role)
            .await?;

        match self
            .ensure_agent_container_with_source(&agent, AgentStatus::Idle, &container_source, None)
            .await
        {
            Ok(_) => Ok(agent.summary.read().await.clone()),
            Err(err) => {
                let message = err.to_string();
                let agent_id = agent.summary.read().await.id;
                if let Err(store_err) = self
                    .set_status(&agent, AgentStatus::Failed, Some(message.clone()))
                    .await
                {
                    tracing::warn!("failed to persist agent failure: {store_err}");
                }
                self.events
                    .publish(ServiceEventKind::Error {
                        agent_id: Some(agent_id),
                        session_id: None,
                        turn_id: None,
                        message,
                    })
                    .await;
                Err(err)
            }
        }
    }

    async fn create_agent_record(
        self: &Arc<Self>,
        request: CreateAgentRequest,
        task_id: Option<TaskId>,
        project_id: Option<ProjectId>,
        role: Option<AgentRole>,
    ) -> Result<Arc<AgentRecord>> {
        let id = Uuid::new_v4();
        let created_at = Utc::now();
        let name = request
            .name
            .unwrap_or_else(|| format!("agent-{}", short_id(id)));
        let provider_selection = self
            .deps
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
            project_id,
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
        self.deps
            .store
            .save_agent(&summary, system_prompt.as_deref())
            .await?;
        let session = agents::initial_session_record(task_id.is_some());
        self.deps
            .store
            .save_agent_session(id, &session.summary)
            .await?;

        let agent = Arc::new(AgentRecord {
            summary: RwLock::new(summary.clone()),
            sessions: Mutex::new(vec![session]),
            container: RwLock::new(None),
            mcp: RwLock::new(None),
            system_prompt,
            turn_lock: Mutex::new(()),
            cancel_requested: AtomicBool::new(false),
            active_turn: StdMutex::new(None),
            pending_inputs: Mutex::new(VecDeque::new()),
        });

        self.state
            .agents
            .write()
            .await
            .insert(id, Arc::clone(&agent));
        self.events
            .publish(ServiceEventKind::AgentCreated {
                agent: summary.clone(),
            })
            .await;
        Ok(agent)
    }

    pub async fn list_agents(&self) -> Vec<AgentSummary> {
        let agents = self.state.agents.read().await.values().cloned().collect();
        agents::list_agents(agents).await
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
        let provider_selection = self.deps.store.resolve_provider(provider_id, model).await?;
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
        self.events
            .publish(ServiceEventKind::AgentUpdated {
                agent: updated.clone(),
            })
            .await;
        Ok(updated)
    }

    pub async fn cleanup_orphaned_containers(&self) -> Result<Vec<String>> {
        let (active_agent_ids, active_project_ids) = {
            let agents = self.state.agents.read().await;
            let projects = self.state.projects.read().await;
            (
                agents
                    .keys()
                    .map(ToString::to_string)
                    .collect::<HashSet<_>>(),
                projects
                    .keys()
                    .map(ToString::to_string)
                    .collect::<HashSet<_>>(),
            )
        };
        Ok(self
            .deps
            .docker
            .cleanup_orphaned_managed_containers(&active_agent_ids, &active_project_ids)
            .await?)
    }

    pub async fn get_agent(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> Result<AgentDetail> {
        agents::get_agent(self, agent_id, session_id, AUTO_COMPACT_THRESHOLD_PERCENT).await
    }

    pub async fn create_session(&self, agent_id: AgentId) -> Result<AgentSessionSummary> {
        agents::create_session(self, agent_id).await
    }

    pub async fn tool_trace(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        call_id: String,
    ) -> Result<ToolTraceDetail> {
        if let Some(trace) = self
            .deps
            .store
            .load_tool_trace(agent_id, session_id, &call_id)
            .await?
        {
            return Ok(trace);
        }
        let agent = self.agent(agent_id).await?;
        let (session_id, history) = {
            let sessions = agent.sessions.lock().await;
            let selected_session =
                agents::selected_session(&sessions, session_id).ok_or_else(|| {
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
            .events
            .tool_metadata(agent_id, session_id, &call_id)
            .await;
        Ok(ToolTraceDetail {
            agent_id,
            session_id: Some(session_id),
            turn_id: None,
            call_id,
            tool_name,
            arguments: arguments.unwrap_or_else(|| json!({})),
            success: event_success.unwrap_or(!output.is_empty()),
            output_preview: turn::tools::trace_preview_output(&output, 500),
            output,
            duration_ms,
            started_at: None,
            completed_at: None,
            output_artifacts: Vec::new(),
        })
    }

    pub async fn tool_output_artifact(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        call_id: String,
        artifact_id: String,
    ) -> Result<(ToolOutputArtifactInfo, PathBuf)> {
        let trace = self
            .tool_trace(agent_id, session_id, call_id.clone())
            .await?;
        let artifact = trace
            .output_artifacts
            .into_iter()
            .find(|artifact| artifact.id == artifact_id && artifact.call_id == call_id)
            .ok_or_else(|| {
                RuntimeError::InvalidInput("tool output artifact not found".to_string())
            })?;
        let path = self.tool_output_artifact_file_path(
            artifact.agent_id,
            &artifact.call_id,
            &artifact.id,
            &artifact.name,
        );
        Ok((artifact, path))
    }

    pub async fn agent_logs(
        &self,
        agent_id: AgentId,
        filter: AgentLogFilter,
    ) -> Result<AgentLogsResponse> {
        self.agent(agent_id).await?;
        Ok(AgentLogsResponse {
            logs: self.deps.store.list_agent_logs(agent_id, filter).await?,
        })
    }

    pub async fn tool_traces(
        &self,
        agent_id: AgentId,
        filter: ToolTraceFilter,
    ) -> Result<ToolTraceListResponse> {
        self.agent(agent_id).await?;
        Ok(ToolTraceListResponse {
            tool_calls: self.deps.store.list_tool_traces(agent_id, filter).await?,
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
        let (agent, turn_id) = self.prepare_turn(agent_id).await?;
        self.spawn_turn(
            &agent,
            agent_id,
            session_id,
            turn_id,
            message,
            skill_mentions,
        );
        Ok(turn_id)
    }

    async fn prepare_turn(&self, agent_id: AgentId) -> Result<(Arc<AgentRecord>, TurnId)> {
        agents::prepare_turn(self, agent_id).await
    }

    fn spawn_turn(
        self: &Arc<Self>,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
    ) {
        let runtime = Arc::clone(self);
        let cancellation_token = CancellationToken::new();
        let task_token = cancellation_token.clone();
        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        let control = TurnControl {
            turn_id,
            session_id,
            cancellation_token,
            abort_handle: Some(abort_handle),
        };
        *agent.active_turn.lock().expect("active turn lock") = Some(control);
        tokio::spawn(Abortable::new(
            async move {
                runtime
                    .run_turn(
                        agent_id,
                        session_id,
                        turn_id,
                        message,
                        skill_mentions,
                        task_token,
                    )
                    .await;
            },
            abort_registration,
        ));
    }

    pub async fn cancel_agent(self: &Arc<Self>, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        let turn_id = agent.summary.read().await.current_turn;
        match turn_id {
            Some(turn_id) => self.cancel_agent_turn(agent_id, turn_id).await,
            None => {
                agent.cancel_requested.store(true, Ordering::SeqCst);
                self.set_status(&agent, AgentStatus::Cancelled, None).await
            }
        }
    }

    pub async fn cancel_agent_turn(
        self: &Arc<Self>,
        agent_id: AgentId,
        turn_id: TurnId,
    ) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        let control = agent.active_turn.lock().expect("active turn lock").clone();
        let current_turn = agent.summary.read().await.current_turn;
        if current_turn != Some(turn_id)
            && control.as_ref().map(|turn| turn.turn_id) != Some(turn_id)
        {
            return Ok(());
        }
        agent.cancel_requested.store(true, Ordering::SeqCst);
        if let Some(control) = control.filter(|turn| turn.turn_id == turn_id) {
            control.cancellation_token.cancel();
            if let Some(abort_handle) = control.abort_handle {
                let token = control.cancellation_token.clone();
                tokio::spawn(async move {
                    sleep(TURN_CANCEL_GRACE).await;
                    if token.is_cancelled() {
                        abort_handle.abort();
                    }
                });
            }
        }
        let completed = turn::completion::complete_turn_if_current(
            self.deps.store.as_ref(),
            &self.events,
            &agent,
            agent_id,
            TurnResult {
                turn_id,
                status: TurnStatus::Cancelled,
                agent_status: AgentStatus::Cancelled,
                final_text: None,
                error: None,
            },
        )
        .await?;
        if completed {
            self.start_next_queued_input_after_turn(agent_id).await;
        }
        Ok(())
    }

    pub async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
        let targets = self.descendant_delete_order(agent_id).await?;
        for target_id in targets {
            self.delete_agent_record(target_id).await?;
        }
        Ok(())
    }

    async fn close_agent(&self, agent_id: AgentId) -> Result<AgentStatus> {
        agents::close_agent(self, agent_id).await
    }

    async fn resume_agent(&self, agent_id: AgentId) -> Result<AgentSummary> {
        agents::resume_agent(self, agent_id).await
    }

    pub async fn cancel_task(self: &Arc<Self>, task_id: TaskId) -> Result<()> {
        let task = self.task(task_id).await?;
        let agents = self.task_agents(task_id).await;
        for agent in agents {
            if let Ok(record) = self.agent(agent.id).await {
                let current_turn = record.summary.read().await.current_turn;
                if let Some(turn_id) = current_turn {
                    let _ = self.cancel_agent_turn(agent.id, turn_id).await;
                } else {
                    record.cancel_requested.store(true, Ordering::SeqCst);
                    let _ = self.set_status(&record, AgentStatus::Cancelled, None).await;
                }
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
        self.deps.store.delete_task(task_id).await?;
        self.state.tasks.write().await.remove(&task_id);
        self.events
            .publish(ServiceEventKind::TaskDeleted { task_id })
            .await;
        Ok(())
    }

    async fn delete_agent_record(&self, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        let reviewer_project_id = {
            let summary = agent.summary.read().await;
            (summary.role == Some(AgentRole::Reviewer))
                .then_some(summary.project_id)
                .flatten()
        };
        agent.cancel_requested.store(true, Ordering::SeqCst);
        self.set_status(&agent, AgentStatus::DeletingContainer, None)
            .await?;
        if let Some(control) = agent.active_turn.lock().expect("active turn lock").clone() {
            control.cancellation_token.cancel();
            if let Some(abort_handle) = control.abort_handle {
                abort_handle.abort();
            }
        }
        if let Some(manager) = agent.mcp.write().await.take() {
            manager.shutdown().await;
        }
        let in_memory_container_id = agent
            .container
            .write()
            .await
            .take()
            .map(|container| container.id);
        let persisted_container_id = agent.summary.read().await.container_id.clone();
        let preferred_container_id = in_memory_container_id.or(persisted_container_id);
        let deleted = self
            .deps
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
        if let Some(project_id) = reviewer_project_id
            && let Err(err) = self
                .cleanup_project_review_worktree(project_id, agent_id)
                .await
        {
            tracing::warn!(
                project_id = %project_id,
                reviewer_id = %agent_id,
                "failed to clean project reviewer worktree during agent deletion: {err}"
            );
        }
        let _turn_guard = agent.turn_lock.lock().await;
        self.set_status(&agent, AgentStatus::Deleted, None).await?;
        self.deps.store.delete_agent(agent_id).await?;
        self.state.agents.write().await.remove(&agent_id);
        self.events
            .publish(ServiceEventKind::AgentDeleted { agent_id })
            .await;
        Ok(())
    }

    async fn descendant_delete_order(&self, root_id: AgentId) -> Result<Vec<AgentId>> {
        let summaries = {
            let agents = self.state.agents.read().await;
            let mut summaries = Vec::with_capacity(agents.len());
            for agent in agents.values() {
                summaries.push(agent.summary.read().await.clone());
            }
            summaries
        };
        if !summaries.iter().any(|summary| summary.id == root_id) {
            return Err(RuntimeError::AgentNotFound(root_id));
        }

        Ok(agents::descendant_delete_order_from_summaries(
            root_id, &summaries,
        ))
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
        self.deps
            .docker
            .copy_to_container(&container_id, temp.path(), &path)
            .await?;
        Ok(bytes.len())
    }

    pub async fn download_file_tar(&self, agent_id: AgentId, path: String) -> Result<Vec<u8>> {
        let container_id = self.container_id(agent_id).await?;
        Ok(self
            .deps
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
        let task_id = agent
            .summary
            .read()
            .await
            .task_id
            .ok_or_else(|| RuntimeError::InvalidInput("Agent has no task".to_string()))?;
        let container_id = self.container_id(agent_id).await?;

        let name = display_name.unwrap_or_else(|| {
            Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone())
        });
        let name = safe_artifact_name(&name)?;

        let artifact_id = Uuid::new_v4().to_string();
        let dir = self.artifact_file_dir(task_id, &artifact_id);
        std::fs::create_dir_all(&dir)?;

        let dest = dir.join(&name);
        self.deps
            .docker
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

        self.deps.store.save_artifact(&info)?;

        let task = self.task(task_id).await?;
        task.artifacts.write().await.push(info.clone());

        self.events
            .publish(ServiceEventKind::ArtifactCreated {
                artifact: info.clone(),
            })
            .await;

        Ok(info)
    }

    pub fn artifact_file_path(&self, info: &ArtifactInfo) -> PathBuf {
        self.artifact_file_dir(info.task_id, &info.id)
            .join(&info.name)
    }

    pub fn tool_output_artifact_file_path(
        &self,
        agent_id: AgentId,
        call_id: &str,
        artifact_id: &str,
        name: &str,
    ) -> PathBuf {
        turn::tools::tool_output_artifact_file_path(
            &self.artifact_files_root,
            agent_id,
            call_id,
            artifact_id,
            name,
        )
    }

    async fn run_turn(
        self: Arc<Self>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
        cancellation_token: CancellationToken,
    ) {
        turn::orchestrator::run_turn(
            &self.deps,
            &self.state,
            &self.events,
            &self,
            agent_id,
            session_id,
            turn_id,
            message,
            skill_mentions,
            cancellation_token,
        )
        .await;
    }

    #[cfg(test)]
    async fn run_turn_inner(
        self: &Arc<Self>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        turn::orchestrator::run_turn_inner(
            &self.deps,
            &self.state,
            &self.events,
            self,
            agent_id,
            session_id,
            turn_id,
            message,
            skill_mentions,
            cancellation_token,
        )
        .await
    }

    async fn execute_tool(
        self: &Arc<Self>,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        turn_id: TurnId,
        name: &str,
        arguments: Value,
        cancellation_token: CancellationToken,
    ) -> Result<ToolExecution> {
        let context = turn::tools::ToolDispatchContext {
            state: &self.state,
            container: turn::tools::ContainerToolContext {
                docker: &self.deps.docker,
                artifact_files_root: &self.artifact_files_root,
                ops: self,
            },
            events: &self.events,
            ops: self,
        };
        turn::tools::execute_tool(
            &context,
            agent,
            agent_id,
            turn_id,
            name,
            arguments,
            cancellation_token,
        )
        .await
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
                self.deps
                    .store
                    .save_plan_history_entry(task_id, &entry)
                    .await?;
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
            self.deps.store.save_task(&task_summary, &plan).await?;
            self.events
                .publish(ServiceEventKind::PlanUpdated {
                    task_id,
                    plan: plan.clone(),
                })
                .await;
            self.events
                .publish(ServiceEventKind::TaskUpdated {
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
            self.deps.store.append_task_review(&review).await?;
            reviews.push(review.clone());
            review
        };
        {
            let plan = task.plan.read().await.clone();
            let mut summary = task.summary.write().await;
            summary.review_rounds = task.reviews.read().await.len() as u64;
            summary.updated_at = now();
            self.refresh_task_summary_counts(&mut summary).await;
            self.deps.store.save_task(&summary, &plan).await?;
            self.events
                .publish(ServiceEventKind::TaskUpdated {
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
            if matches!(
                executor_summary.status,
                AgentStatus::Failed | AgentStatus::Cancelled
            ) {
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
                system_prompt: Some(agents::task_role_system_prompt(role).to_string()),
            },
            ContainerSource::CloneFrom {
                parent_container_id,
                docker_image: parent_summary.docker_image,
                workspace_volume: None,
            },
            Some(task_id),
            parent_summary.project_id,
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
        let (agent, turn_id) = self.prepare_turn(agent_id).await?;
        self.spawn_turn(&agent, agent_id, session_id, turn_id, message, Vec::new());
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
        self.execute_tool(
            &agent,
            agent_id,
            Uuid::new_v4(),
            name,
            arguments,
            CancellationToken::new(),
        )
        .await
    }

    async fn wait_agent(&self, agent_id: AgentId, timeout: Duration) -> Result<AgentSummary> {
        agents::wait_agent(self, agent_id, timeout).await
    }

    async fn wait_agent_until_complete_with_cancel(
        &self,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> Result<AgentSummary> {
        agents::wait_agent_until_complete_with_cancel(self, agent_id, cancellation_token).await
    }

    async fn agent_wait_snapshot(&self, agent_id: AgentId) -> Result<Value> {
        let agent = self.agent(agent_id).await?;
        let summary = agent.summary.read().await.clone();
        let (session_id, recent_messages) = self.agent_recent_messages(agent_id, 12).await?;
        let last_message = recent_messages.last().cloned();
        let tracked_response = {
            let sessions = agent.sessions.lock().await;
            agents::last_turn_response(&sessions)
        };
        let final_response =
            agents::final_wait_response(&summary, &recent_messages, tracked_response);
        let recent_events = self.agent_recent_events(agent_id, 12).await;
        let last_activity_at = agents::last_activity_at(&summary, &recent_messages, &recent_events);
        let active_tool = agents::active_tool_snapshot(&recent_events);
        let idle_ms = (now() - last_activity_at).num_milliseconds().max(0) as u64;
        let diagnostics = json!({
            "current_turn": summary.current_turn,
            "active_tool": active_tool.clone(),
            "last_error": summary.last_error.clone(),
            "idle_ms": idle_ms,
            "recent_events": recent_events.clone(),
        });
        Ok(json!({
            "agent": summary.clone(),
            "agent_id": agent_id,
            "name": summary.name.clone(),
            "role": summary.role.clone(),
            "status": summary.status.clone(),
            "current_turn": summary.current_turn,
            "updated_at": summary.updated_at,
            "last_activity_at": last_activity_at,
            "last_message": last_message,
            "session_id": session_id,
            "final_response": final_response,
            "recent_messages": recent_messages,
            "recent_events": recent_events,
            "active_tool": active_tool,
            "diagnostics": diagnostics,
        }))
    }

    async fn agent_recent_events(&self, agent_id: AgentId, limit: usize) -> Vec<ServiceEvent> {
        self.events.recent_for_agent(agent_id, limit).await
    }

    async fn wait_agents_output_with_cancel(
        &self,
        agent_ids: Vec<AgentId>,
        timeout: Duration,
        cancellation_token: &CancellationToken,
    ) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancellation_token.is_cancelled() {
                return Err(RuntimeError::TurnCancelled);
            }
            let mut completed = Vec::new();
            let mut pending = Vec::new();
            for agent_id in &agent_ids {
                let summary = self.agent(*agent_id).await?.summary.read().await.clone();
                if agents::is_agent_wait_complete(&summary) {
                    completed.push(*agent_id);
                } else {
                    pending.push(*agent_id);
                }
            }
            if !completed.is_empty() || pending.is_empty() || Instant::now() >= deadline {
                let mut completed_outputs = Vec::new();
                for agent_id in completed {
                    completed_outputs.push(self.agent_wait_snapshot(agent_id).await?);
                    self.cleanup_finished_explorer_agent(agent_id).await?;
                }
                let mut pending_outputs = Vec::new();
                for agent_id in pending {
                    pending_outputs.push(self.agent_wait_snapshot(agent_id).await?);
                }
                return Ok(json!({
                    "completed": completed_outputs,
                    "pending": pending_outputs,
                    "timed_out": !pending_outputs.is_empty(),
                }));
            }
            tokio::select! {
                _ = sleep(Duration::from_millis(250)) => {},
                _ = cancellation_token.cancelled() => return Err(RuntimeError::TurnCancelled),
            }
        }
    }

    async fn send_input_to_agent(
        self: &Arc<Self>,
        target: AgentId,
        session_id: Option<SessionId>,
        message: String,
        skill_mentions: Vec<String>,
        interrupt: bool,
    ) -> Result<Value> {
        agents::send_input_to_agent(
            self.as_ref(),
            self,
            target,
            session_id,
            message,
            skill_mentions,
            interrupt,
            TURN_CANCEL_GRACE,
        )
        .await
    }

    async fn start_next_queued_input_after_turn(self: &Arc<Self>, agent_id: AgentId) {
        agents::start_next_queued_input_after_turn(self.as_ref(), self, agent_id).await;
    }

    async fn fork_agent_context(&self, parent_id: AgentId, child_id: AgentId) -> Result<()> {
        agents::fork_agent_context(self, parent_id, child_id).await
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
        Ok(agents::recent_messages(&sessions, limit))
    }

    async fn build_instructions(
        &self,
        agent: &AgentRecord,
        skills_manager: &SkillsManager,
        skill_injections: &SkillInjections,
        skills_config: &SkillsConfigRequest,
        mcp_tools: &[mai_mcp::McpTool],
        container_skill_paths: &ContainerSkillPaths,
    ) -> Result<String> {
        let summary = agent.summary.read().await;
        let project_id = summary.project_id;
        let prefer_container_skill_paths = summary.role == Some(AgentRole::Reviewer);
        drop(summary);
        let skills_response = if let Some(project_id) = project_id {
            let mut response = skills_manager.list(skills_config)?;
            self.apply_project_skill_source_paths(project_id, &mut response);
            response
        } else {
            skills_manager.list(skills_config)?
        };
        Ok(instructions::build_instructions(
            agent.system_prompt.as_deref(),
            skills_response,
            skill_injections,
            mcp_tools,
            container_skill_paths,
            prefer_container_skill_paths,
        ))
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
        self.events
            .publish(ServiceEventKind::AgentStatusChanged { agent_id, status })
            .await;
        Ok(())
    }

    async fn set_turn_status(
        &self,
        agent: &Arc<AgentRecord>,
        turn_id: TurnId,
        cancellation_token: &CancellationToken,
        enforce_current_turn: bool,
        status: AgentStatus,
    ) -> Result<()> {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        let agent_id = {
            let mut summary = agent.summary.write().await;
            if enforce_current_turn && summary.current_turn != Some(turn_id) {
                return Err(RuntimeError::TurnCancelled);
            }
            summary.status = status.clone();
            summary.updated_at = now();
            summary.id
        };
        self.persist_agent(agent).await?;
        self.events
            .publish(ServiceEventKind::AgentStatusChanged { agent_id, status })
            .await;
        Ok(())
    }

    async fn maybe_auto_compact(
        self: &Arc<Self>,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        cancellation_token: &CancellationToken,
    ) -> Result<()> {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        let last_context_tokens =
            turn::history::session_context_tokens(agent, agent_id, session_id).await?;
        let Some(tokens_before) = last_context_tokens else {
            return Ok(());
        };
        let summary = agent.summary.read().await.clone();
        let provider_selection = self
            .deps
            .store
            .resolve_provider(Some(&summary.provider_id), Some(&summary.model))
            .await?;
        if !should_auto_compact(tokens_before, provider_selection.model.context_tokens) {
            return Ok(());
        }

        let history =
            turn::history::session_history(self.deps.store.as_ref(), agent, agent_id, session_id)
                .await?;
        if history.is_empty() {
            turn::history::record_session_context_tokens(
                self.deps.store.as_ref(),
                agent,
                agent_id,
                session_id,
                0,
            )
            .await?;
            return Ok(());
        }
        let mut compact_input = history.clone();
        compact_input.push(ModelInputItem::user_text(COMPACT_PROMPT));
        let skills_config = self.deps.store.load_skills_config().await?;
        let skills_manager = self.skills_manager_for_agent(agent).await?;
        let instructions = {
            let _project_skill_guard = self.project_skill_read_guard(agent).await;
            self.build_instructions(
                agent,
                &skills_manager,
                &SkillInjections::default(),
                &skills_config,
                &[],
                &ContainerSkillPaths::default(),
            )
            .await?
        };
        let resolved = self.deps.model.resolve(
            &provider_selection.provider,
            &provider_selection.model,
            summary.reasoning_effort.as_deref(),
        );
        let response = turn::model_stream::consume_model_stream_to_response(
            &self.deps.model,
            &resolved,
            &instructions,
            &compact_input,
            &[],
            &mut ModelTurnState::default(),
            cancellation_token,
        )
        .await
        .map_err(turn::model_stream::model_error_to_runtime)?;

        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }

        if let Some(usage) = response.usage {
            {
                let mut summary = agent.summary.write().await;
                summary.token_usage.add(&usage);
                summary.updated_at = now();
            }
            self.persist_agent(agent).await?;
        }

        let summary_text = turn::history::compact_summary_from_output(&response.output)
            .ok_or_else(|| {
                RuntimeError::InvalidInput("compact response did not include a summary".to_string())
            })?;
        let replacement = turn::history::build_compacted_history(
            &history,
            &summary_text,
            COMPACT_USER_MESSAGE_MAX_CHARS,
            COMPACT_SUMMARY_PREFIX,
        );
        turn::history::replace_session_history(
            self.deps.store.as_ref(),
            agent,
            agent_id,
            session_id,
            replacement,
        )
        .await?;
        self.events
            .publish(ServiceEventKind::ContextCompacted {
                agent_id,
                session_id,
                turn_id,
                tokens_before,
                summary_preview: preview(&summary_text, COMPACT_SUMMARY_PREVIEW_CHARS),
            })
            .await;
        turn::persistence::record_agent_log(
            self.deps.store.as_ref(),
            agent_id,
            Some(session_id),
            Some(turn_id),
            "info",
            "context",
            "context compacted",
            json!({
                "tokens_before": tokens_before,
                "summary_preview": preview(&summary_text, COMPACT_SUMMARY_PREVIEW_CHARS),
            }),
        )
        .await;
        Ok(())
    }

    async fn persist_agent(&self, agent: &AgentRecord) -> Result<()> {
        let summary = agent.summary.read().await.clone();
        self.deps
            .store
            .save_agent(&summary, agent.system_prompt.as_deref())
            .await?;
        Ok(())
    }

    async fn task(&self, task_id: TaskId) -> Result<Arc<TaskRecord>> {
        self.state
            .tasks
            .read()
            .await
            .get(&task_id)
            .cloned()
            .ok_or(RuntimeError::TaskNotFound(task_id))
    }

    async fn project(&self, project_id: ProjectId) -> Result<Arc<ProjectRecord>> {
        projects::service::project(&self.state, project_id).await
    }

    async fn project_skill_lock(&self, project_id: ProjectId) -> Arc<RwLock<()>> {
        self.state
            .project_skill_locks
            .write()
            .await
            .entry(project_id)
            .or_insert_with(|| Arc::new(RwLock::new(())))
            .clone()
    }

    async fn project_agents(&self, project_id: ProjectId) -> Vec<AgentSummary> {
        projects::service::project_agents(&self.state, project_id).await
    }

    async fn project_auto_reviewer_agents(&self, project_id: ProjectId) -> Vec<AgentSummary> {
        projects::service::project_auto_reviewer_agents(&self.state, project_id).await
    }

    async fn project_skills_from_cache(&self, project_id: ProjectId) -> Result<SkillsListResponse> {
        let lock = self.project_skill_lock(project_id).await;
        projects::skills::list_from_cache(&self.deps.store, &self.cache_root, &lock, project_id)
            .await
    }

    fn skills_manager_with_project_roots(&self, project_id: ProjectId) -> SkillsManager {
        self.deps
            .skills
            .clone_with_extra_roots(self.project_skill_roots(project_id))
    }

    async fn skills_manager_for_agent(&self, agent: &AgentRecord) -> Result<SkillsManager> {
        let project_id = agent.summary.read().await.project_id;
        Ok(project_id
            .map(|project_id| self.skills_manager_with_project_roots(project_id))
            .unwrap_or_else(|| self.deps.skills.clone()))
    }

    async fn project_skill_read_guard(
        &self,
        agent: &AgentRecord,
    ) -> Option<tokio::sync::OwnedRwLockReadGuard<()>> {
        let project_id = agent.summary.read().await.project_id?;
        let lock = self.project_skill_lock(project_id).await;
        Some(lock.read_owned().await)
    }

    async fn refresh_project_skills_for_agent(&self, agent: &AgentRecord) -> Result<()> {
        let Some(project_id) = agent.summary.read().await.project_id else {
            return Ok(());
        };
        self.refresh_project_skills_from_project_sidecar_if_ready(project_id)
            .await
    }

    async fn sync_agent_skills_to_container(
        &self,
        agent: &Arc<AgentRecord>,
        skills_manager: &SkillsManager,
        skills_config: &SkillsConfigRequest,
    ) -> Result<ContainerSkillPaths> {
        let agent_id = agent.summary.read().await.id;
        let container_id = self.container_id(agent_id).await?;
        let _project_skill_guard = self.project_skill_read_guard(agent).await;
        let mut response = skills_manager.list(skills_config)?;
        if let Some(project_id) = agent.summary.read().await.project_id {
            self.apply_project_skill_source_paths(project_id, &mut response);
        }
        let skills = response
            .skills
            .into_iter()
            .filter(|skill| {
                skill.enabled
                    && matches!(skill.scope, SkillScope::System | SkillScope::Project)
                    && skill.path.parent().is_some()
            })
            .collect::<Vec<_>>();
        if skills.is_empty() {
            return Ok(ContainerSkillPaths::default());
        }

        let cleanup = self
            .deps
            .docker
            .exec_shell(
                &container_id,
                &format!(
                    "rm -rf {root} && mkdir -p {root}",
                    root = shell_quote(CONTAINER_SKILLS_ROOT)
                ),
                Some("/"),
                Some(10),
            )
            .await
            .map_err(|err| {
                RuntimeError::InvalidInput(format!(
                    "failed to sync skills into agent container: {err}"
                ))
            })?;
        if cleanup.status != 0 {
            let message = preview(
                format!("{}\n{}", cleanup.stderr, cleanup.stdout).trim(),
                500,
            );
            return Err(RuntimeError::InvalidInput(format!(
                "failed to sync skills into agent container: {message}"
            )));
        }

        let mut mapped = HashMap::new();
        let mut copied_dirs = HashSet::new();
        for skill in skills {
            let Some(skill_dir) = skill.path.parent() else {
                continue;
            };
            let container_dir = instructions::container_skill_dir(&skill);
            if copied_dirs.insert(container_dir.clone()) {
                self.deps
                    .docker
                    .copy_to_container(&container_id, skill_dir, &container_dir.to_string_lossy())
                    .await
                    .map_err(|err| {
                        RuntimeError::InvalidInput(format!(
                            "failed to sync skills into agent container: {err}"
                        ))
                    })?;
            }
            mapped.insert(skill.path, container_dir.join("SKILL.md"));
        }
        Ok(ContainerSkillPaths::from_paths(mapped))
    }

    async fn refresh_project_skills_from_project_sidecar_if_ready(
        &self,
        project_id: ProjectId,
    ) -> Result<()> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        if summary.status != ProjectStatus::Ready
            || summary.clone_status != ProjectCloneStatus::Ready
        {
            return Ok(());
        }
        let sidecar = self.ensure_project_sidecar(project_id).await?;
        let existing = self.existing_project_skill_dirs(&sidecar.id).await?;
        self.refresh_project_skill_cache(
            project_id,
            ProjectSkillRefreshSource::ProjectSidecar,
            Some(&sidecar.id),
            &existing,
        )
        .await
    }

    async fn refresh_project_skills_from_review_workspace(
        &self,
        project_id: ProjectId,
    ) -> Result<()> {
        let sources = self
            .existing_project_skill_dirs_in_review_workspace(project_id)
            .await?;
        self.refresh_project_skill_cache(
            project_id,
            ProjectSkillRefreshSource::ReviewWorkspace,
            None,
            &sources,
        )
        .await
    }

    fn project_skill_cache_dir(&self, project_id: ProjectId) -> PathBuf {
        projects::skills::cache_dir(&self.cache_root, project_id)
    }

    fn artifact_file_dir(&self, task_id: TaskId, artifact_id: &str) -> PathBuf {
        self.artifact_files_root
            .join(task_id.to_string())
            .join(artifact_id)
    }

    fn project_skill_roots(&self, project_id: ProjectId) -> Vec<(PathBuf, SkillScope)> {
        projects::skills::roots_for_project(&self.cache_root, project_id)
    }

    fn apply_project_skill_source_paths(
        &self,
        project_id: ProjectId,
        response: &mut SkillsListResponse,
    ) {
        projects::skills::apply_project_source_paths(&self.cache_root, project_id, response);
    }

    async fn existing_project_skill_dirs(
        &self,
        container_id: &str,
    ) -> Result<Vec<ProjectSkillSourceDir>> {
        projects::skills::detect_existing_dirs_in_container(&self.deps.docker, container_id).await
    }

    async fn existing_project_skill_dirs_in_review_workspace(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<ProjectSkillSourceDir>> {
        projects::skills::detect_existing_dirs_in_review_workspace(
            &self.deps.docker,
            &self.sidecar_image,
            project_id,
        )
        .await
    }

    async fn refresh_project_skill_cache(
        &self,
        project_id: ProjectId,
        source: ProjectSkillRefreshSource,
        container_id: Option<&str>,
        sources: &[ProjectSkillSourceDir],
    ) -> Result<()> {
        let lock = self.project_skill_lock(project_id).await;
        projects::skills::refresh_cache(
            &self.deps.docker,
            &self.sidecar_image,
            &self.cache_root,
            &lock,
            project_id,
            source,
            container_id,
            sources,
        )
        .await
    }

    async fn ensure_project_sidecar(&self, project_id: ProjectId) -> Result<ContainerHandle> {
        let project = self.project(project_id).await?;
        if let Some(container) = project.sidecar.read().await.clone() {
            return Ok(container);
        }

        let mut sidecar_guard = project.sidecar.write().await;
        if let Some(container) = sidecar_guard.clone() {
            return Ok(container);
        }

        let workspace_volume = project_workspace_volume(&project_id.to_string());
        let container = self
            .deps
            .docker
            .ensure_project_sidecar_container(
                &project_id.to_string(),
                None,
                &self.sidecar_image,
                &workspace_volume,
                &ContainerCreateOptions::default(),
            )
            .await?;
        *sidecar_guard = Some(container.clone());
        Ok(container)
    }

    async fn ensure_project_mcp_manager(
        &self,
        project_id: ProjectId,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> Result<Option<Arc<McpAgentManager>>> {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        if let Some(manager) = self
            .state
            .project_mcp_managers
            .read()
            .await
            .get(&project_id)
            .cloned()
        {
            return Ok(Some(manager));
        }

        let Some(token) = self.project_git_token(project_id).await? else {
            return Ok(None);
        };
        let sidecar = self.ensure_project_sidecar(project_id).await?;
        let configs = projects::mcp::project_mcp_configs(&token);
        self.events
            .publish(ServiceEventKind::McpServerStatusChanged {
                agent_id,
                server: "project".to_string(),
                status: mai_protocol::McpStartupStatus::Starting,
                error: None,
            })
            .await;
        let manager = McpAgentManager::start(self.deps.docker.clone(), sidecar.id, configs).await;
        if cancellation_token.is_cancelled() {
            manager.shutdown().await;
            return Err(RuntimeError::TurnCancelled);
        }
        for status in manager.statuses().await {
            let error = status.error.map(|error| redact_secret(&error, &token));
            self.events
                .publish(ServiceEventKind::McpServerStatusChanged {
                    agent_id,
                    server: status.server,
                    status: status.status,
                    error,
                })
                .await;
        }
        let manager = Arc::new(manager);
        let mut managers = self.state.project_mcp_managers.write().await;
        if let Some(existing) = managers.get(&project_id).cloned() {
            manager.shutdown().await;
            return Ok(Some(existing));
        }
        managers.insert(project_id, Arc::clone(&manager));
        Ok(Some(manager))
    }

    async fn project_git_token(&self, project_id: ProjectId) -> Result<Option<String>> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let Some(account_id) = summary.git_account_id else {
            return Ok(None);
        };
        Ok(Some(self.deps.git_accounts.token(&account_id).await?))
    }

    async fn project_mcp_manager_for_agent(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> Result<Option<Arc<McpAgentManager>>> {
        let Some(project_id) = agent.summary.read().await.project_id else {
            return Ok(None);
        };
        self.ensure_project_mcp_manager(project_id, agent_id, cancellation_token)
            .await
    }

    async fn shutdown_project_mcp_manager(&self, project_id: ProjectId) {
        if let Some(manager) = self
            .state
            .project_mcp_managers
            .write()
            .await
            .remove(&project_id)
        {
            manager.shutdown().await;
        }
    }

    async fn delete_project_sidecar(&self, project_id: ProjectId) -> Result<Vec<String>> {
        let project = match self.project(project_id).await {
            Ok(project) => project,
            Err(RuntimeError::ProjectNotFound(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let preferred_container_id = project
            .sidecar
            .write()
            .await
            .take()
            .map(|container| container.id);
        let deleted = self
            .deps
            .docker
            .delete_project_sidecar_containers(
                &project_id.to_string(),
                preferred_container_id.as_deref(),
            )
            .await?;
        if !deleted.is_empty() {
            tracing::info!(
                project_id = %project_id,
                count = deleted.len(),
                "removed project sidecar containers"
            );
        }
        Ok(deleted)
    }

    async fn set_project_clone_result(
        &self,
        project_id: ProjectId,
        status: ProjectStatus,
        clone_status: ProjectCloneStatus,
        last_error: Option<String>,
    ) -> Result<ProjectSummary> {
        let project = self.project(project_id).await?;
        let updated = {
            let mut summary = project.summary.write().await;
            summary.status = status;
            summary.clone_status = clone_status;
            summary.last_error = last_error;
            summary.updated_at = now();
            summary.clone()
        };
        self.deps.store.save_project(&updated).await?;
        self.events
            .publish(ServiceEventKind::ProjectUpdated {
                project: updated.clone(),
            })
            .await;
        Ok(updated)
    }

    async fn start_project_workspace(
        self: &Arc<Self>,
        project_id: ProjectId,
        maintainer_agent_id: AgentId,
    ) -> Result<()> {
        let setup_result = async {
            let maintainer = self.agent(maintainer_agent_id).await?;
            self.ensure_agent_container_with_source(
                &maintainer,
                AgentStatus::Idle,
                &ContainerSource::FreshImage,
                None,
            )
            .await?;
            self.set_project_clone_result(
                project_id,
                ProjectStatus::Creating,
                ProjectCloneStatus::Cloning,
                None,
            )
            .await?;
            self.clone_project_repository(project_id, maintainer_agent_id)
                .await
        }
        .await;

        let update = match setup_result {
            Ok(()) => {
                self.set_project_clone_result(
                    project_id,
                    ProjectStatus::Ready,
                    ProjectCloneStatus::Ready,
                    None,
                )
                .await
            }
            Err(err) => {
                self.shutdown_project_mcp_manager(project_id).await;
                let _ = self.delete_project_sidecar(project_id).await;
                self.set_project_clone_result(
                    project_id,
                    ProjectStatus::Failed,
                    ProjectCloneStatus::Failed,
                    Some(err.to_string()),
                )
                .await
            }
        };
        if let Err(err) = update {
            tracing::warn!(project_id = %project_id, "failed to update project clone status: {err}");
            return Err(err);
        }
        self.start_project_review_loop_if_ready(project_id).await?;
        Ok(())
    }

    async fn start_enabled_project_review_workers(self: &Arc<Self>) {
        let project_ids = {
            let projects = self.state.projects.read().await;
            projects.keys().copied().collect::<Vec<_>>()
        };
        for project_id in project_ids {
            if let Err(err) = self.start_project_review_loop_if_ready(project_id).await {
                tracing::warn!(project_id = %project_id, "failed to start project review loop: {err}");
            }
        }
    }

    async fn reconcile_project_review_singletons(self: &Arc<Self>) {
        let project_ids = {
            let projects = self.state.projects.read().await;
            projects.keys().copied().collect::<Vec<_>>()
        };
        for project_id in project_ids {
            if let Err(err) = self.reconcile_project_review_singleton(project_id).await {
                tracing::warn!(project_id = %project_id, "failed to reconcile project reviewer singleton: {err}");
            }
        }
    }

    async fn reconcile_project_review_singleton(
        self: &Arc<Self>,
        project_id: ProjectId,
    ) -> Result<()> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let mut stale_reviewer_ids = HashSet::new();
        if let Some(reviewer_id) = summary.current_reviewer_agent_id {
            stale_reviewer_ids.insert(reviewer_id);
        }

        let runs = self
            .deps
            .store
            .load_project_review_runs(project_id, None, 0, PROJECT_REVIEW_RUN_LIST_LIMIT)
            .await?;
        let mut has_stale_activity = summary.current_reviewer_agent_id.is_some();
        for run in runs {
            if run.finished_at.is_some()
                || !matches!(
                    run.status,
                    ProjectReviewRunStatus::Syncing | ProjectReviewRunStatus::Running
                )
            {
                continue;
            }
            has_stale_activity = true;
            if let Some(reviewer_id) = run.reviewer_agent_id {
                stale_reviewer_ids.insert(reviewer_id);
            }
            let _ = projects::review::runs::finish_project_review_run(
                &self.deps.store,
                self.as_ref(),
                FinishReviewRun {
                    run_id: run.id,
                    project_id,
                    reviewer_agent_id: run.reviewer_agent_id,
                    turn_id: run.turn_id,
                    status: ProjectReviewRunStatus::Cancelled,
                    outcome: None,
                    pr: run.pr,
                    summary_text: run.summary,
                    error: Some("review interrupted by server restart".to_string()),
                },
            )
            .await;
        }

        for agent in self.project_auto_reviewer_agents(project_id).await {
            has_stale_activity = true;
            stale_reviewer_ids.insert(agent.id);
        }

        for reviewer_id in stale_reviewer_ids {
            if let Err(err) = self.delete_agent(reviewer_id).await {
                tracing::warn!(
                    project_id = %project_id,
                    reviewer_id = %reviewer_id,
                    "failed to delete stale project reviewer agent: {err}"
                );
            }
        }

        if has_stale_activity {
            let status = if summary.auto_review_enabled {
                ProjectReviewStatus::Idle
            } else {
                ProjectReviewStatus::Disabled
            };
            let _ = self
                .set_project_review_state(project_id, status, ReviewStateUpdate::default())
                .await?;
        }
        Ok(())
    }

    async fn start_project_review_loop_if_ready(
        self: &Arc<Self>,
        project_id: ProjectId,
    ) -> Result<()> {
        let project = self.project(project_id).await?;
        let should_start = {
            let summary = project.summary.read().await;
            summary.auto_review_enabled
                && summary.status == ProjectStatus::Ready
                && summary.clone_status == ProjectCloneStatus::Ready
        };
        if !should_start {
            return Ok(());
        }

        let mut worker = project.review_worker.lock().await;
        if worker.is_some() {
            return Ok(());
        }
        let cancellation_token = CancellationToken::new();
        let runtime = Arc::clone(self);
        let token = cancellation_token.clone();
        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        tokio::spawn(Abortable::new(
            async move {
                runtime.run_project_review_loop(project_id, token).await;
            },
            abort_registration,
        ));
        *worker = Some(ProjectReviewWorker {
            cancellation_token,
            abort_handle,
        });
        Ok(())
    }

    async fn stop_project_review_loop(self: &Arc<Self>, project_id: ProjectId) {
        let project = match self.project(project_id).await {
            Ok(project) => project,
            Err(_) => return,
        };
        let worker = project.review_worker.lock().await.take();
        if let Some(worker) = worker {
            worker.cancellation_token.cancel();
            worker.abort_handle.abort();
        }
        let reviewer_id = project.summary.read().await.current_reviewer_agent_id;
        let _ = projects::review::runs::cancel_active_project_review_runs(
            &self.deps.store,
            self.as_ref(),
            project_id,
            reviewer_id,
            PROJECT_REVIEW_RUN_LIST_LIMIT,
        )
        .await;
        if let Some(reviewer_id) = reviewer_id {
            if let Ok(agent) = self.agent(reviewer_id).await {
                let current_turn = agent.summary.read().await.current_turn;
                if let Some(turn_id) = current_turn {
                    let _ = self.cancel_agent_turn(reviewer_id, turn_id).await;
                }
            }
            let _ = self.delete_agent(reviewer_id).await;
        }
        let _ = self
            .set_project_review_state(
                project_id,
                ProjectReviewStatus::Disabled,
                ReviewStateUpdate {
                    force_disabled: true,
                    ..Default::default()
                },
            )
            .await;
    }

    async fn run_project_review_loop(
        self: Arc<Self>,
        project_id: ProjectId,
        cancellation_token: CancellationToken,
    ) {
        if let Err(err) = self.ensure_project_review_workspace(project_id).await {
            let _ = projects::review::runs::record_project_review_startup_failure(
                &self.deps.store,
                project_id,
                err.to_string(),
            )
            .await;
            let next = Utc::now() + TimeDelta::seconds(PROJECT_REVIEW_FAILURE_RETRY_SECS as i64);
            let _ = self
                .set_project_review_state(
                    project_id,
                    ProjectReviewStatus::Failed,
                    ReviewStateUpdate {
                        next_review_at: Some(next),
                        error: Some(err.to_string()),
                        ..Default::default()
                    },
                )
                .await;
        }
        loop {
            if cancellation_token.is_cancelled() {
                break;
            }
            let should_continue = match self.project(project_id).await {
                Ok(project) => {
                    let summary = project.summary.read().await;
                    summary.auto_review_enabled
                        && summary.status == ProjectStatus::Ready
                        && summary.clone_status == ProjectCloneStatus::Ready
                }
                Err(_) => false,
            };
            if !should_continue {
                break;
            }

            let decision = self
                .run_project_review_once(project_id, cancellation_token.clone(), None)
                .await;
            let decision = match decision {
                Ok(result) => projects::review::project_review_loop_decision_for_result(result),
                Err(RuntimeError::TurnCancelled) if cancellation_token.is_cancelled() => break,
                Err(err) => {
                    projects::review::project_review_loop_decision_for_error(err.to_string())
                }
            };
            let next_review_at = (decision.delay.as_secs() > 0)
                .then(|| Utc::now() + TimeDelta::seconds(decision.delay.as_secs() as i64));
            let _ = self
                .set_project_review_state(
                    project_id,
                    decision.status,
                    ReviewStateUpdate {
                        next_review_at,
                        outcome: decision.outcome,
                        summary_text: decision.summary,
                        error: decision.error,
                        ..Default::default()
                    },
                )
                .await;
            if decision.delay.is_zero() {
                continue;
            }
            tokio::select! {
                _ = sleep(decision.delay) => {}
                _ = cancellation_token.cancelled() => break,
            }
        }
        if let Ok(project) = self.project(project_id).await {
            let mut worker = project.review_worker.lock().await;
            *worker = None;
        }
    }

    async fn run_project_review_once(
        self: &Arc<Self>,
        project_id: ProjectId,
        cancellation_token: CancellationToken,
        target_pr: Option<u64>,
    ) -> Result<ProjectReviewCycleResult> {
        let run_id = Uuid::new_v4();
        self.set_project_review_state(
            project_id,
            ProjectReviewStatus::Syncing,
            ReviewStateUpdate::default(),
        )
        .await?;
        projects::review::runs::save_project_review_run_status(
            &self.deps.store,
            ProjectReviewRunSummary {
                id: run_id,
                project_id,
                reviewer_agent_id: None,
                turn_id: None,
                started_at: now(),
                finished_at: None,
                status: ProjectReviewRunStatus::Syncing,
                outcome: None,
                pr: target_pr,
                summary: None,
                error: None,
            },
            Vec::new(),
            Vec::new(),
        )
        .await?;
        if let Err(err) = self.sync_project_review_repo(project_id).await {
            let error = err.to_string();
            projects::review::runs::finish_project_review_run(
                &self.deps.store,
                self.as_ref(),
                FinishReviewRun {
                    run_id,
                    project_id,
                    reviewer_agent_id: None,
                    turn_id: None,
                    status: ProjectReviewRunStatus::Failed,
                    outcome: Some(ProjectReviewOutcome::Failed),
                    pr: target_pr,
                    summary_text: None,
                    error: Some(error),
                },
            )
            .await?;
            return Err(err);
        }
        if let Err(err) = self
            .refresh_project_skills_from_review_workspace(project_id)
            .await
        {
            let error = err.to_string();
            projects::review::runs::finish_project_review_run(
                &self.deps.store,
                self.as_ref(),
                FinishReviewRun {
                    run_id,
                    project_id,
                    reviewer_agent_id: None,
                    turn_id: None,
                    status: ProjectReviewRunStatus::Failed,
                    outcome: Some(ProjectReviewOutcome::Failed),
                    pr: target_pr,
                    summary_text: None,
                    error: Some(error),
                },
            )
            .await?;
            return Err(err);
        }
        if cancellation_token.is_cancelled() {
            projects::review::runs::finish_project_review_run(
                &self.deps.store,
                self.as_ref(),
                FinishReviewRun {
                    run_id,
                    project_id,
                    reviewer_agent_id: None,
                    turn_id: None,
                    status: ProjectReviewRunStatus::Cancelled,
                    outcome: None,
                    pr: target_pr,
                    summary_text: None,
                    error: Some("review cancelled".to_string()),
                },
            )
            .await?;
            return Err(RuntimeError::TurnCancelled);
        }
        let reviewer = match self.spawn_project_reviewer_agent(project_id).await {
            Ok(reviewer) => reviewer,
            Err(err) => {
                projects::review::runs::finish_project_review_run(
                    &self.deps.store,
                    self.as_ref(),
                    FinishReviewRun {
                        run_id,
                        project_id,
                        reviewer_agent_id: None,
                        turn_id: None,
                        status: ProjectReviewRunStatus::Failed,
                        outcome: Some(ProjectReviewOutcome::Failed),
                        pr: target_pr,
                        summary_text: None,
                        error: Some(err.to_string()),
                    },
                )
                .await?;
                return Err(err);
            }
        };
        let reviewer_id = reviewer.id;
        self.set_project_review_state(
            project_id,
            ProjectReviewStatus::Running,
            ReviewStateUpdate {
                current_reviewer_agent_id: Some(reviewer_id),
                ..Default::default()
            },
        )
        .await?;
        let started_at = self
            .deps
            .store
            .load_project_review_run(project_id, run_id)
            .await?
            .map(|run| run.summary.started_at)
            .unwrap_or_else(now);
        projects::review::runs::save_project_review_run_status(
            &self.deps.store,
            ProjectReviewRunSummary {
                id: run_id,
                project_id,
                reviewer_agent_id: Some(reviewer_id),
                turn_id: None,
                started_at,
                finished_at: None,
                status: ProjectReviewRunStatus::Running,
                outcome: None,
                pr: target_pr,
                summary: None,
                error: None,
            },
            Vec::new(),
            Vec::new(),
        )
        .await?;
        let cycle_result = async {
            let message = self
                .project_reviewer_initial_message(project_id, reviewer_id, target_pr)
                .await?;
            let turn_id = self
                .start_agent_turn_with_skills(
                    reviewer_id,
                    message,
                    vec!["reviewer-agent-review-pr".to_string()],
                )
                .await?;
            projects::review::runs::update_project_review_run_turn(
                &self.deps.store,
                project_id,
                run_id,
                reviewer_id,
                turn_id,
            )
            .await?;
            let summary = self
                .wait_agent_until_complete_with_cancel(reviewer_id, &cancellation_token)
                .await?;
            if summary.status == AgentStatus::Cancelled && cancellation_token.is_cancelled() {
                return Err(RuntimeError::TurnCancelled);
            }
            if let Some(result) =
                projects::review::project_review_cycle_result_for_reviewer_status(&summary)
            {
                return Ok(result);
            }
            let response = self.last_turn_response(reviewer_id).await?.ok_or_else(|| {
                RuntimeError::InvalidInput("reviewer did not return a final response".to_string())
            })?;
            projects::review::parse_project_review_cycle_report(&response)
        }
        .await;
        let turn_id = self
            .deps
            .store
            .load_project_review_run(project_id, run_id)
            .await?
            .and_then(|run| run.summary.turn_id);
        let (status, outcome, pr, summary, error) = match &cycle_result {
            Ok(result) => {
                let status = if result.outcome == ProjectReviewOutcome::Failed {
                    ProjectReviewRunStatus::Failed
                } else {
                    ProjectReviewRunStatus::Completed
                };
                (
                    status,
                    Some(result.outcome.clone()),
                    result.pr,
                    result.summary.clone(),
                    result.error.clone(),
                )
            }
            Err(RuntimeError::TurnCancelled) if cancellation_token.is_cancelled() => (
                ProjectReviewRunStatus::Cancelled,
                None,
                None,
                None,
                Some("review cancelled".to_string()),
            ),
            Err(err) => (
                ProjectReviewRunStatus::Failed,
                Some(ProjectReviewOutcome::Failed),
                None,
                None,
                Some(err.to_string()),
            ),
        };
        let _ = projects::review::runs::finish_project_review_run(
            &self.deps.store,
            self.as_ref(),
            FinishReviewRun {
                run_id,
                project_id,
                reviewer_agent_id: Some(reviewer_id),
                turn_id,
                status,
                outcome,
                pr,
                summary_text: summary,
                error,
            },
        )
        .await;
        let _ = self
            .cleanup_project_review_worktree(project_id, reviewer_id)
            .await;
        let _ = self.delete_agent(reviewer_id).await;
        self.set_project_review_state(
            project_id,
            ProjectReviewStatus::Idle,
            ReviewStateUpdate::default(),
        )
        .await?;
        cycle_result
    }

    async fn ensure_project_review_workspace(&self, project_id: ProjectId) -> Result<()> {
        self.run_project_review_repo_command(
            project_id,
            projects::review::workspace::ReviewRepoCommand::Ensure,
        )
        .await
    }

    async fn sync_project_review_repo(&self, project_id: ProjectId) -> Result<()> {
        self.run_project_review_repo_command(
            project_id,
            projects::review::workspace::ReviewRepoCommand::Sync,
        )
        .await
    }

    async fn run_project_review_cleanup_loop(self: Arc<Self>) {
        if let Err(err) = self.cleanup_project_review_history().await {
            tracing::warn!("project review cleanup failed: {err}");
        }
        loop {
            sleep(Duration::from_secs(PROJECT_REVIEW_CLEANUP_INTERVAL_SECS)).await;
            if let Err(err) = self.cleanup_project_review_history().await {
                tracing::warn!("project review cleanup failed: {err}");
            }
        }
    }

    async fn cleanup_project_review_history(&self) -> Result<()> {
        let cutoff = Utc::now() - TimeDelta::days(PROJECT_REVIEW_HISTORY_RETENTION_DAYS);
        let removed_runs = self
            .deps
            .store
            .prune_project_review_runs_before(cutoff)
            .await?;
        let removed_events = self.deps.store.prune_service_events_before(cutoff).await?;
        let removed_logs = self.deps.store.prune_agent_logs_before(cutoff).await?;
        let removed_traces = self.deps.store.prune_tool_traces_before(cutoff).await?;
        if removed_runs > 0 || removed_events > 0 || removed_logs > 0 || removed_traces > 0 {
            tracing::info!(
                removed_runs,
                removed_events,
                removed_logs,
                removed_traces,
                "pruned project review history"
            );
        }
        self.events.retain_since(cutoff).await;
        let projects = self.list_projects().await;
        for project in projects {
            if let Err(err) = self
                .cleanup_project_review_workspace_history(project.id, cutoff)
                .await
            {
                tracing::warn!(project_id = %project.id, "failed to clean project review workspace history: {err}");
            }
        }
        Ok(())
    }

    async fn cleanup_project_review_workspace_history(
        &self,
        project_id: ProjectId,
        cutoff: DateTime<Utc>,
    ) -> Result<()> {
        let active_reviewer = match self.project(project_id).await {
            Ok(project) => project.summary.read().await.current_reviewer_agent_id,
            Err(_) => None,
        };
        projects::review::workspace::cleanup_history(
            &self.deps.docker,
            &self.sidecar_image,
            project_id,
            active_reviewer,
            cutoff,
        )
        .await
    }

    async fn cleanup_project_review_worktree(
        &self,
        project_id: ProjectId,
        reviewer_id: AgentId,
    ) -> Result<()> {
        projects::review::workspace::cleanup_worktree(
            &self.deps.docker,
            &self.sidecar_image,
            project_id,
            reviewer_id,
        )
        .await
    }

    async fn run_project_review_repo_command(
        &self,
        project_id: ProjectId,
        command: projects::review::workspace::ReviewRepoCommand,
    ) -> Result<()> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let token = self.project_git_token(project_id).await?.ok_or_else(|| {
            RuntimeError::InvalidInput("project git account token is not configured".to_string())
        })?;
        projects::review::workspace::run_repo_command(
            &self.deps.docker,
            &self.sidecar_image,
            &summary,
            &token,
            command,
        )
        .await
    }

    async fn spawn_project_reviewer_agent(
        self: &Arc<Self>,
        project_id: ProjectId,
    ) -> Result<AgentSummary> {
        let project = self.project(project_id).await?;
        let project_summary = project.summary.read().await.clone();
        let maintainer = self.agent(project_summary.maintainer_agent_id).await?;
        let maintainer_summary = maintainer.summary.read().await.clone();
        let model = self.resolve_role_agent_model(AgentRole::Reviewer).await?;
        let workspace_volume = project_review_workspace_volume(&project_id.to_string());
        self.create_agent_with_container_source(
            CreateAgentRequest {
                name: Some(format!("{} Auto Reviewer", project_summary.name)),
                provider_id: Some(model.preference.provider_id),
                model: Some(model.preference.model),
                reasoning_effort: model.preference.reasoning_effort,
                docker_image: Some(maintainer_summary.docker_image.clone()),
                parent_id: Some(project_summary.maintainer_agent_id),
                system_prompt: Some(projects::review::project_reviewer_system_prompt().to_string()),
            },
            ContainerSource::ImageWithWorkspace { workspace_volume },
            maintainer_summary.task_id,
            Some(project_id),
            Some(AgentRole::Reviewer),
        )
        .await
    }

    async fn project_reviewer_initial_message(
        &self,
        project_id: ProjectId,
        reviewer_id: AgentId,
        target_pr: Option<u64>,
    ) -> Result<String> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let extra = summary
            .reviewer_extra_prompt
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("None");
        let target = target_pr
            .map(|pr| format!("Target pull request: review PR #{pr} only. Do not select another pull request. Use `select-pr --target-pr {pr}` when invoking the helper."))
            .unwrap_or_else(|| {
                "Target pull request: none. Select exactly one eligible pull request using the helper."
                    .to_string()
            });
        Ok(format!(
            "Run one automatic pull request review for project `{}`.\n\nRepository: {}/{}\nDefault branch: {}\nWorkspace repo: /workspace/repo\nReview worktree root: /workspace/reviews/{}\n{}\n\nExtra reviewer instructions:\n{}\n\nUse the $reviewer-agent-review-pr skill. At the end of the turn, return only one JSON object matching this schema exactly:\n{{\"outcome\":\"review_submitted|no_eligible_pr|failed\",\"pr\":123|null,\"summary\":\"short result\",\"error\":null|\"failure reason\"}}",
            summary.name, summary.owner, summary.repo, summary.branch, reviewer_id, target, extra
        ))
    }

    async fn last_turn_response(&self, agent_id: AgentId) -> Result<Option<String>> {
        let agent = self.agent(agent_id).await?;
        let sessions = agent.sessions.lock().await;
        Ok(agents::last_turn_response(&sessions))
    }

    async fn start_agent_turn_with_skills(
        self: &Arc<Self>,
        agent_id: AgentId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        let session_id = self.resolve_session_id(agent_id, None).await?;
        let (agent, turn_id) = self.prepare_turn(agent_id).await?;
        self.spawn_turn(
            &agent,
            agent_id,
            session_id,
            turn_id,
            message,
            skill_mentions,
        );
        Ok(turn_id)
    }

    async fn set_project_review_state(
        &self,
        project_id: ProjectId,
        status: ProjectReviewStatus,
        update: ReviewStateUpdate,
    ) -> Result<ProjectSummary> {
        let project = self.project(project_id).await?;
        let updated = {
            let mut summary = project.summary.write().await;
            summary.review_status = status;
            summary.current_reviewer_agent_id = update.current_reviewer_agent_id;
            summary.next_review_at = update.next_review_at;
            if update.current_reviewer_agent_id.is_some() {
                summary.last_review_started_at = Some(now());
                summary.last_review_finished_at = None;
            } else if update.outcome.is_some() || update.error.is_some() {
                summary.last_review_finished_at = Some(now());
            }
            if let Some(outcome) = update.outcome {
                summary.last_review_outcome = Some(outcome);
            }
            summary.review_last_error = update.error;
            if update.force_disabled {
                summary.auto_review_enabled = false;
            }
            summary.updated_at = now();
            summary.clone()
        };
        self.deps.store.save_project(&updated).await?;
        self.events
            .publish(ServiceEventKind::ProjectUpdated {
                project: updated.clone(),
            })
            .await;
        Ok(updated)
    }

    async fn delete_project_review_workspace(&self, project_id: ProjectId) -> Result<()> {
        let volume = project_review_workspace_volume(&project_id.to_string());
        self.deps.docker.delete_volume(&volume).await?;
        Ok(())
    }

    async fn project_git_token_for_agent(&self, agent: &AgentRecord) -> Result<Option<String>> {
        let Some(project_id) = agent.summary.read().await.project_id else {
            return Ok(None);
        };
        self.project_git_token(project_id).await
    }

    async fn execute_project_mcp_tool(
        &self,
        agent: &AgentRecord,
        model_name: &str,
        arguments: Value,
        cancellation_token: CancellationToken,
    ) -> Result<ToolExecution> {
        let agent_id = agent.summary.read().await.id;
        let Some(manager) = self
            .project_mcp_manager_for_agent(agent, agent_id, &cancellation_token)
            .await?
        else {
            return Err(RuntimeError::InvalidInput(
                "project MCP manager is not available".to_string(),
            ));
        };
        let token = self
            .project_git_token_for_agent(agent)
            .await?
            .unwrap_or_default();
        let summary = agent.summary.read().await.clone();
        let arguments = projects::review::project_review_mcp_arguments_with_model_footer(
            model_name,
            arguments,
            summary.role.as_ref(),
            &summary.model,
        );
        let output = tokio::select! {
            output = manager.call_model_tool(model_name, arguments) => output,
            _ = cancellation_token.cancelled() => {
                return Err(RuntimeError::TurnCancelled);
            }
        };
        let output = output.map_err(|err| match err {
            mai_mcp::McpError::ToolNotFound(_) => RuntimeError::InvalidInput(format!(
                "project MCP tool `{model_name}` was not discovered"
            )),
            other => RuntimeError::InvalidInput(redact_secret(&other.to_string(), &token)),
        })?;
        Ok(ToolExecution::new(
            true,
            redact_secret(&output.to_string(), &token),
            false,
        ))
    }

    async fn agent_resource_broker(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> Result<AgentResourceBroker> {
        let agent_mcp = agent.mcp.read().await.clone();
        let project_mcp = if agent.summary.read().await.project_id.is_some() {
            self.project_mcp_manager_for_agent(agent, agent_id, cancellation_token)
                .await
                .unwrap_or(None)
        } else {
            None
        };
        let project_skill_guard = self.project_skill_read_guard(agent).await;
        let skills_config = self.deps.store.load_skills_config().await?;
        let skills = {
            self.skills_manager_for_agent(agent)
                .await?
                .list(&skills_config)?
        };
        Ok(AgentResourceBroker {
            agent_mcp,
            project_mcp,
            skills,
            _project_skill_guard: project_skill_guard,
        })
    }

    async fn execute_project_github_api_get(
        &self,
        agent: &AgentRecord,
        path: &str,
    ) -> Result<ToolExecution> {
        let Some(token) = self.project_git_token_for_agent(agent).await? else {
            return Err(RuntimeError::InvalidInput(
                "agent is not attached to a project".to_string(),
            ));
        };
        let path = normalize_github_api_get_path(path)?;
        let url = github_api_url(&self.github_api_base_url, &path);
        let response = self
            .deps
            .github_http
            .get(url)
            .bearer_auth(&token)
            .headers(github_headers())
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let output = if status.is_success() {
            serde_json::from_str::<Value>(&text)
                .unwrap_or_else(|_| json!({ "status": status.as_u16(), "body": text }))
        } else {
            let message = serde_json::from_str::<GithubErrorResponse>(&text)
                .ok()
                .and_then(|error| error.message)
                .filter(|message| !message.trim().is_empty())
                .unwrap_or_else(|| preview(&text, 300));
            json!({
                "status": status.as_u16(),
                "error": redact_secret(&message, &token),
            })
        };
        Ok(ToolExecution::new(
            status.is_success(),
            redact_secret(&output.to_string(), &token),
            false,
        ))
    }

    async fn clone_project_repository(
        &self,
        project_id: ProjectId,
        _maintainer_agent_id: AgentId,
    ) -> Result<()> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let account_id = summary.git_account_id.clone().ok_or_else(|| {
            RuntimeError::InvalidInput("project git account is not configured".to_string())
        })?;
        let token = self.deps.git_accounts.token(&account_id).await?;
        let repo_url = github_clone_url(&summary.owner, &summary.repo);
        let sidecar = self.ensure_project_sidecar(project_id).await?;
        self.clone_repository_in_sidecar(&sidecar.id, &repo_url, summary.branch.trim(), &token)
            .await?;
        self.prepare_copied_project_workspace(&sidecar.id).await?;
        let existing = self.existing_project_skill_dirs(&sidecar.id).await?;
        self.refresh_project_skill_cache(
            project_id,
            ProjectSkillRefreshSource::ProjectSidecar,
            Some(&sidecar.id),
            &existing,
        )
        .await?;
        Ok(())
    }

    async fn prepare_copied_project_workspace(&self, container_id: &str) -> Result<()> {
        let command = format!(
            "set -eu\n\
             owner=$(id -u):$(id -g)\n\
             chown -R \"$owner\" {workspace} 2>/dev/null || git config --global --add safe.directory {workspace}",
            workspace = shell_quote(PROJECT_WORKSPACE_PATH),
        );
        let output = self
            .deps
            .docker
            .exec_shell(container_id, &command, Some("/"), Some(60))
            .await?;
        if output.status != 0 {
            let combined = format!("{}\n{}", output.stderr, output.stdout);
            let message = preview(combined.trim(), 500);
            return Err(RuntimeError::InvalidInput(format!(
                "repository workspace ownership setup failed: {message}"
            )));
        }
        Ok(())
    }

    async fn clone_repository_in_sidecar(
        &self,
        container_id: &str,
        repo_url: &str,
        branch: &str,
        token: &str,
    ) -> Result<()> {
        let branch_arg = if branch.is_empty() {
            String::new()
        } else {
            format!(" --branch {}", shell_quote(branch))
        };
        let command = format!(
            "set -eu\n\
             tmp=$(mktemp -d)\n\
             askpass=\"$tmp/askpass.sh\"\n\
             cleanup() {{ rm -rf \"$tmp\"; }}\n\
             trap cleanup EXIT HUP INT TERM\n\
             cat >\"$askpass\" <<'EOF'\n\
#!/bin/sh\n\
case \"$1\" in\n\
  *Username*) printf '%s\\n' x-access-token ;;\n\
  *Password*) printf '%s\\n' \"$MAI_GITHUB_INSTALLATION_TOKEN\" ;;\n\
  *) printf '\\n' ;;\n\
esac\n\
EOF\n\
             chmod 700 \"$askpass\"\n\
             rm -rf {workspace}\n\
             GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=\"$askpass\" git -c credential.helper= clone{branch_arg} -- {repo_url} {workspace}",
            workspace = shell_quote(PROJECT_WORKSPACE_PATH),
            repo_url = shell_quote(repo_url),
        );
        let output = self
            .deps
            .docker
            .exec_shell_env(
                container_id,
                &command,
                Some("/"),
                Some(600),
                &[(
                    "MAI_GITHUB_INSTALLATION_TOKEN".to_string(),
                    token.to_string(),
                )],
            )
            .await?;
        if output.status != 0 {
            let combined = format!("{}\n{}", output.stderr, output.stdout);
            let message = preview(redact_secret(combined.trim(), token).trim(), 500);
            return Err(RuntimeError::InvalidInput(format!(
                "repository clone failed in project sidecar: {message}"
            )));
        }
        Ok(())
    }

    async fn task_summary(&self, task: &Arc<TaskRecord>) -> TaskSummary {
        let mut summary = task.summary.read().await.clone();
        self.refresh_task_summary_counts(&mut summary).await;
        summary
    }

    async fn refresh_task_summary_counts(&self, summary: &mut TaskSummary) {
        summary.agent_count = self.task_agents(summary.id).await.len();
        let task = {
            let tasks = self.state.tasks.read().await;
            tasks.get(&summary.id).cloned()
        };
        if let Some(task) = task {
            summary.review_rounds = task.reviews.read().await.len() as u64;
        }
    }

    async fn task_agents(&self, task_id: TaskId) -> Vec<AgentSummary> {
        let agents = self.state.agents.read().await;
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
        self.deps.store.save_task(&summary, &plan).await?;
        self.events
            .publish(ServiceEventKind::TaskUpdated {
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
        self.deps.store.save_task(&summary, &plan).await?;
        self.events
            .publish(ServiceEventKind::TaskUpdated {
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
        agents::selected_session(&sessions, session_id)
            .map(|session| session.summary.id)
            .ok_or_else(|| RuntimeError::SessionNotFound {
                agent_id,
                session_id: session_id.unwrap_or_default(),
            })
    }

    async fn resolve_role_agent_model(&self, role: AgentRole) -> Result<ResolvedAgentModel> {
        let config = self.deps.store.load_agent_config().await?;
        let preference = role_preference(&config, role);
        match self.resolve_agent_model_preference(role, preference).await {
            Ok(resolved) => Ok(resolved),
            Err(err) if preference.is_some() && is_stale_agent_model_preference_error(&err) => {
                tracing::warn!(
                    role = agent_role_label(role),
                    error = %err,
                    "agent role model preference is stale; falling back to the default provider"
                );
                self.resolve_agent_model_preference(role, None).await
            }
            Err(err) => Err(err),
        }
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
            .deps
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

    async fn ensure_agent_container(
        &self,
        agent: &Arc<AgentRecord>,
        ready_status: AgentStatus,
    ) -> Result<String> {
        self.ensure_agent_container_with_source(
            agent,
            ready_status,
            &ContainerSource::FreshImage,
            None,
        )
        .await
    }

    async fn ensure_agent_container_for_turn(
        &self,
        agent: &Arc<AgentRecord>,
        ready_status: AgentStatus,
        turn_id: TurnId,
        cancellation_token: &CancellationToken,
    ) -> Result<String> {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        let turn_guard =
            (agent.summary.read().await.current_turn == Some(turn_id)).then(|| TurnGuard {
                turn_id,
                cancellation_token: cancellation_token.clone(),
            });
        let container_id = self
            .ensure_agent_container_with_source(
                agent,
                ready_status.clone(),
                &ContainerSource::FreshImage,
                turn_guard,
            )
            .await?;
        let current_turn = agent.summary.read().await.current_turn;
        if cancellation_token.is_cancelled()
            || current_turn.is_some_and(|current| current != turn_id)
        {
            if let Some(manager) = agent.mcp.write().await.take() {
                manager.shutdown().await;
            }
            return Err(RuntimeError::TurnCancelled);
        }
        let needs_status_restore = agent.summary.read().await.status != ready_status;
        if needs_status_restore {
            self.set_status(agent, ready_status, None).await?;
        }
        Ok(container_id)
    }

    async fn ensure_agent_container_with_source(
        &self,
        agent: &Arc<AgentRecord>,
        ready_status: AgentStatus,
        container_source: &ContainerSource,
        turn_guard: Option<TurnGuard>,
    ) -> Result<String> {
        if let Some(guard) = &turn_guard {
            self.ensure_turn_current(agent, guard).await?;
        }
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
        if let Some(guard) = &turn_guard {
            self.ensure_turn_current(agent, guard).await?;
        }
        let container_result = match container_source {
            ContainerSource::FreshImage => {
                self.deps
                    .docker
                    .ensure_agent_container_from_image(
                        &agent_id.to_string(),
                        preferred_container_id.as_deref(),
                        &docker_image,
                    )
                    .await
            }
            ContainerSource::ImageWithWorkspace { workspace_volume } => {
                self.deps
                    .docker
                    .ensure_agent_container_from_image_with_workspace(
                        &agent_id.to_string(),
                        preferred_container_id.as_deref(),
                        &docker_image,
                        Some(workspace_volume),
                    )
                    .await
            }
            ContainerSource::CloneFrom {
                parent_container_id,
                docker_image,
                workspace_volume,
            } => {
                if preferred_container_id.is_some() && workspace_volume.is_none() {
                    self.deps
                        .docker
                        .ensure_agent_container_from_image(
                            &agent_id.to_string(),
                            preferred_container_id.as_deref(),
                            docker_image,
                        )
                        .await
                } else {
                    self.deps
                        .docker
                        .create_agent_container_from_parent_with_workspace(
                            &agent_id.to_string(),
                            parent_container_id,
                            workspace_volume.as_deref(),
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
        if let Some(guard) = &turn_guard
            && let Err(err) = self.ensure_turn_current(agent, guard).await
        {
            drop(container_guard);
            let _ = self
                .deps
                .docker
                .delete_agent_containers(&agent_id.to_string(), Some(&container_id))
                .await;
            return Err(err);
        }
        {
            let mut summary = agent.summary.write().await;
            summary.container_id = Some(container_id.clone());
            summary.updated_at = now();
        }
        self.persist_agent(agent).await?;
        *container_guard = Some(container.clone());
        drop(container_guard);

        let mcp_configs = self
            .deps
            .store
            .list_mcp_servers()
            .await?
            .into_iter()
            .filter(|(_, config)| config.scope == McpServerScope::Agent)
            .collect::<std::collections::BTreeMap<_, _>>();
        for server in mcp_configs
            .iter()
            .filter_map(|(server, config)| config.enabled.then_some(server))
        {
            self.events
                .publish(ServiceEventKind::McpServerStatusChanged {
                    agent_id,
                    server: server.clone(),
                    status: mai_protocol::McpStartupStatus::Starting,
                    error: None,
                })
                .await;
        }
        let mcp = McpAgentManager::start(self.deps.docker.clone(), container.id, mcp_configs).await;
        if let Some(guard) = &turn_guard
            && let Err(err) = self.ensure_turn_current(agent, guard).await
        {
            mcp.shutdown().await;
            *agent.container.write().await = None;
            {
                let mut summary = agent.summary.write().await;
                summary.container_id = None;
            }
            let _ = self
                .deps
                .docker
                .delete_agent_containers(&agent_id.to_string(), Some(&container_id))
                .await;
            return Err(err);
        }
        for status in mcp.statuses().await {
            self.events
                .publish(ServiceEventKind::McpServerStatusChanged {
                    agent_id,
                    server: status.server,
                    status: status.status,
                    error: status.error,
                })
                .await;
        }
        let required_failures = mcp.required_failures().await;
        if !required_failures.is_empty() {
            let message = required_failures
                .iter()
                .map(|status| {
                    format!(
                        "{}: {}",
                        status.server,
                        status
                            .error
                            .as_deref()
                            .unwrap_or("required MCP server failed")
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            mcp.shutdown().await;
            *agent.container.write().await = None;
            {
                let mut summary = agent.summary.write().await;
                summary.container_id = None;
            }
            let _ = self
                .deps
                .docker
                .delete_agent_containers(&agent_id.to_string(), Some(&container_id))
                .await;
            self.set_status(agent, AgentStatus::Failed, Some(message.clone()))
                .await?;
            return Err(RuntimeError::InvalidInput(format!(
                "required MCP server startup failed: {message}"
            )));
        }
        if let Some(guard) = &turn_guard {
            self.ensure_turn_current(agent, guard).await?;
        }
        *agent.mcp.write().await = Some(Arc::new(mcp));
        self.set_status(agent, ready_status, None).await?;
        Ok(container_id)
    }

    async fn ensure_turn_current(&self, agent: &AgentRecord, guard: &TurnGuard) -> Result<()> {
        if guard.cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        if agent.summary.read().await.current_turn != Some(guard.turn_id) {
            return Err(RuntimeError::TurnCancelled);
        }
        Ok(())
    }

    async fn agent(&self, agent_id: AgentId) -> Result<Arc<AgentRecord>> {
        self.state
            .agents
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
            .unwrap_or_else(|| self.deps.docker.image())
            .to_string()
    }

    async fn agent_mcp_tools(&self, agent: &AgentRecord) -> Vec<mai_mcp::McpTool> {
        if let Some(project_id) = agent.summary.read().await.project_id {
            let Some(manager) = self
                .state
                .project_mcp_managers
                .read()
                .await
                .get(&project_id)
                .cloned()
            else {
                return Vec::new();
            };
            return manager.tools().await;
        }
        let Some(manager) = agent.mcp.read().await.clone() else {
            return Vec::new();
        };
        manager.tools().await
    }

    async fn inject_project_mcp_tools(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        _session_id: SessionId,
        cancellation_token: &CancellationToken,
    ) -> Result<()> {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        if agent.summary.read().await.project_id.is_none() {
            return Ok(());
        }
        let _ = self
            .project_mcp_manager_for_agent(agent, agent_id, cancellation_token)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl agents::AgentServiceOps for AgentRuntime {
    async fn agent(&self, agent_id: AgentId) -> Result<Arc<AgentRecord>> {
        AgentRuntime::agent(self, agent_id).await
    }

    async fn save_agent_session(
        &self,
        agent_id: AgentId,
        session: &AgentSessionSummary,
    ) -> Result<()> {
        self.deps
            .store
            .save_agent_session(agent_id, session)
            .await?;
        Ok(())
    }

    async fn persist_agent(&self, agent: &AgentRecord) -> Result<()> {
        AgentRuntime::persist_agent(self, agent).await
    }

    async fn publish(&self, event: ServiceEventKind) {
        self.events.publish(event).await;
    }

    async fn recent_events_for_agent(&self, agent_id: AgentId) -> Vec<ServiceEvent> {
        self.events.for_agent(agent_id).await
    }

    async fn provider_context_tokens(&self, provider_id: &str, model: &str) -> Option<u64> {
        self.deps
            .store
            .resolve_provider(Some(provider_id), Some(model))
            .await
            .ok()
            .map(|selection| selection.model.context_tokens)
    }

    async fn resolve_session_id(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> Result<SessionId> {
        AgentRuntime::resolve_session_id(self, agent_id, session_id).await
    }

    async fn replace_agent_history(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        history: &[ModelInputItem],
    ) -> Result<()> {
        self.deps
            .store
            .replace_agent_history(agent_id, session_id, history)
            .await?;
        Ok(())
    }

    async fn append_agent_message(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        position: usize,
        message: &AgentMessage,
    ) -> Result<()> {
        self.deps
            .store
            .append_agent_message(agent_id, session_id, position, message)
            .await?;
        Ok(())
    }

    async fn delete_agent_containers(
        &self,
        agent_id: AgentId,
        preferred_container_id: Option<String>,
    ) -> Result<Vec<String>> {
        Ok(self
            .deps
            .docker
            .delete_agent_containers(&agent_id.to_string(), preferred_container_id.as_deref())
            .await?)
    }

    async fn ensure_agent_container(
        &self,
        agent: &Arc<AgentRecord>,
        status: AgentStatus,
    ) -> Result<()> {
        AgentRuntime::ensure_agent_container(self, agent, status)
            .await
            .map(|_| ())
    }
}

impl agents::AgentInputOps for Arc<AgentRuntime> {
    fn cancel_agent_turn(
        &self,
        agent_id: AgentId,
        turn_id: TurnId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::cancel_agent_turn(self, agent_id, turn_id)
    }

    fn set_agent_status(
        &self,
        agent: &Arc<AgentRecord>,
        status: AgentStatus,
        error: Option<String>,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::set_status(self.as_ref(), agent, status, error)
    }

    fn spawn_turn(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
    ) {
        AgentRuntime::spawn_turn(
            self,
            agent,
            agent_id,
            session_id,
            turn_id,
            message,
            skill_mentions,
        );
    }
}

impl projects::review::runs::ReviewRunSnapshotSource for AgentRuntime {
    async fn snapshot(&self, reviewer_agent_id: AgentId) -> (Vec<AgentMessage>, Vec<ServiceEvent>) {
        let messages = self
            .agent_recent_messages(reviewer_agent_id, PROJECT_REVIEW_SNAPSHOT_MESSAGE_LIMIT)
            .await
            .map(|(_, messages)| messages)
            .unwrap_or_default();
        let events = self
            .agent_recent_events(reviewer_agent_id, PROJECT_REVIEW_SNAPSHOT_EVENT_LIMIT)
            .await;
        (messages, events)
    }
}

impl projects::service::ProjectReadOps for AgentRuntime {
    fn get_agent(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> impl std::future::Future<Output = Result<AgentDetail>> + Send {
        AgentRuntime::get_agent(self, agent_id, session_id)
    }

    async fn recent_review_runs(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<ProjectReviewRunSummary>> {
        Ok(self
            .list_project_review_runs(project_id, 0, PROJECT_REVIEW_RUN_LIST_LIMIT)
            .await?
            .runs)
    }
}

#[async_trait]
impl turn::orchestrator::TurnOrchestratorOps for Arc<AgentRuntime> {
    async fn agent(&self, agent_id: AgentId) -> Result<Arc<AgentRecord>> {
        AgentRuntime::agent(self.as_ref(), agent_id).await
    }

    async fn ensure_agent_container_for_turn(
        &self,
        agent: &Arc<AgentRecord>,
        status: AgentStatus,
        turn_id: TurnId,
        cancellation_token: &CancellationToken,
    ) -> Result<()> {
        AgentRuntime::ensure_agent_container_for_turn(
            self.as_ref(),
            agent,
            status,
            turn_id,
            cancellation_token,
        )
        .await
        .map(|_| ())
    }

    async fn refresh_project_skills_for_agent(&self, agent: &AgentRecord) -> Result<()> {
        AgentRuntime::refresh_project_skills_for_agent(self.as_ref(), agent).await
    }

    async fn skills_manager_for_agent(&self, agent: &AgentRecord) -> Result<SkillsManager> {
        AgentRuntime::skills_manager_for_agent(self.as_ref(), agent).await
    }

    async fn sync_agent_skills_to_container(
        &self,
        agent: &Arc<AgentRecord>,
        skills_manager: &SkillsManager,
        skills_config: &SkillsConfigRequest,
    ) -> Result<ContainerSkillPaths> {
        AgentRuntime::sync_agent_skills_to_container(
            self.as_ref(),
            agent,
            skills_manager,
            skills_config,
        )
        .await
    }

    async fn maybe_auto_compact(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        cancellation_token: &CancellationToken,
    ) -> Result<()> {
        AgentRuntime::maybe_auto_compact(
            self,
            agent,
            agent_id,
            session_id,
            turn_id,
            cancellation_token,
        )
        .await
    }

    async fn agent_mcp_tools(&self, agent: &AgentRecord) -> Vec<mai_mcp::McpTool> {
        AgentRuntime::agent_mcp_tools(self.as_ref(), agent).await
    }

    async fn project_skill_read_guard(
        &self,
        agent: &AgentRecord,
    ) -> Option<tokio::sync::OwnedRwLockReadGuard<()>> {
        AgentRuntime::project_skill_read_guard(self.as_ref(), agent).await
    }

    async fn inject_project_mcp_tools(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
        cancellation_token: &CancellationToken,
    ) -> Result<()> {
        AgentRuntime::inject_project_mcp_tools(
            self.as_ref(),
            agent,
            agent_id,
            session_id,
            cancellation_token,
        )
        .await
    }

    async fn build_instructions(
        &self,
        agent: &AgentRecord,
        skills_manager: &SkillsManager,
        skill_injections: &SkillInjections,
        skills_config: &SkillsConfigRequest,
        mcp_tools: &[mai_mcp::McpTool],
        container_skill_paths: &ContainerSkillPaths,
    ) -> Result<String> {
        AgentRuntime::build_instructions(
            self.as_ref(),
            agent,
            skills_manager,
            skill_injections,
            skills_config,
            mcp_tools,
            container_skill_paths,
        )
        .await
    }

    async fn set_turn_status(
        &self,
        agent: &Arc<AgentRecord>,
        turn_id: TurnId,
        cancellation_token: &CancellationToken,
        enforce_current_turn: bool,
        status: AgentStatus,
    ) -> Result<()> {
        AgentRuntime::set_turn_status(
            self.as_ref(),
            agent,
            turn_id,
            cancellation_token,
            enforce_current_turn,
            status,
        )
        .await
    }

    async fn execute_tool(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        turn_id: TurnId,
        name: &str,
        arguments: Value,
        cancellation_token: CancellationToken,
    ) -> Result<ToolExecution> {
        AgentRuntime::execute_tool(
            self,
            agent,
            agent_id,
            turn_id,
            name,
            arguments,
            cancellation_token,
        )
        .await
    }

    async fn persist_agent(&self, agent: &AgentRecord) -> Result<()> {
        AgentRuntime::persist_agent(self.as_ref(), agent).await
    }

    async fn start_next_queued_input_after_turn(&self, agent_id: AgentId) {
        AgentRuntime::start_next_queued_input_after_turn(self, agent_id).await;
    }
}

#[async_trait]
impl turn::tools::ContainerToolOps for Arc<AgentRuntime> {
    async fn container_id(&self, agent_id: AgentId) -> Result<String> {
        AgentRuntime::container_id(self.as_ref(), agent_id).await
    }
}

#[async_trait]
impl turn::tools::ToolDispatchOps for Arc<AgentRuntime> {
    async fn spawn_agent_from_tool(
        &self,
        parent_agent_id: AgentId,
        request: turn::tools::SpawnAgentToolRequest,
    ) -> Result<turn::tools::SpawnAgentToolResult> {
        let parent = self.agent(parent_agent_id).await?;
        let parent_status = parent.summary.read().await.status.clone();
        let parent_summary = parent.summary.read().await.clone();
        let parent_container_id = self.ensure_agent_container(&parent, parent_status).await?;
        let parent_docker_image = parent_summary.docker_image.clone();
        let (provider_id, model, reasoning_effort) = if request.legacy_role.is_some() {
            let child_model = self.resolve_role_agent_model(request.role).await?;
            (
                child_model.preference.provider_id,
                child_model.preference.model,
                child_model.preference.reasoning_effort,
            )
        } else {
            (
                parent_summary.provider_id.clone(),
                request
                    .model
                    .unwrap_or_else(|| parent_summary.model.clone()),
                request
                    .reasoning_effort
                    .or_else(|| parent_summary.reasoning_effort.clone()),
            )
        };
        let created = self
            .create_agent_with_container_source(
                CreateAgentRequest {
                    name: request.name,
                    provider_id: Some(provider_id),
                    model: Some(model),
                    reasoning_effort,
                    docker_image: Some(parent_docker_image.clone()),
                    parent_id: Some(parent_agent_id),
                    system_prompt: Some(agents::task_role_system_prompt(request.role).to_string()),
                },
                ContainerSource::CloneFrom {
                    parent_container_id,
                    docker_image: parent_docker_image,
                    workspace_volume: None,
                },
                parent_summary.task_id,
                parent_summary.project_id,
                Some(request.role),
            )
            .await?;
        if request.fork_context {
            self.fork_agent_context(parent_agent_id, created.id).await?;
        }
        let turn_id = if let Some(message) = request.collab_input.message {
            let session_id = self.resolve_session_id(created.id, None).await?;
            let (agent, turn_id) = self.prepare_turn(created.id).await?;
            self.spawn_turn(
                &agent,
                created.id,
                session_id,
                turn_id,
                message,
                request.collab_input.skill_mentions,
            );
            Some(turn_id)
        } else {
            None
        };
        Ok(turn::tools::SpawnAgentToolResult {
            agent: created,
            turn_id,
        })
    }

    async fn send_input_to_agent(
        &self,
        target: AgentId,
        session_id: Option<SessionId>,
        message: String,
        skill_mentions: Vec<String>,
        interrupt: bool,
    ) -> Result<Value> {
        AgentRuntime::send_input_to_agent(
            self,
            target,
            session_id,
            message,
            skill_mentions,
            interrupt,
        )
        .await
    }

    async fn wait_agents_output_with_cancel(
        &self,
        agent_ids: Vec<AgentId>,
        timeout: Duration,
        cancellation_token: &CancellationToken,
    ) -> Result<Value> {
        AgentRuntime::wait_agents_output_with_cancel(
            self.as_ref(),
            agent_ids,
            timeout,
            cancellation_token,
        )
        .await
    }

    async fn list_agents(&self) -> Vec<AgentSummary> {
        AgentRuntime::list_agents(self.as_ref()).await
    }

    async fn close_agent(&self, agent_id: AgentId) -> Result<AgentStatus> {
        AgentRuntime::close_agent(self.as_ref(), agent_id).await
    }

    async fn resume_agent(&self, agent_id: AgentId) -> Result<AgentSummary> {
        AgentRuntime::resume_agent(self.as_ref(), agent_id).await
    }

    async fn list_mcp_resources(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
        server: Option<String>,
        cursor: Option<String>,
    ) -> Result<Value> {
        let broker = self
            .agent_resource_broker(agent, agent_id, cancellation_token)
            .await?;
        broker.list_resources(server.as_deref(), cursor).await
    }

    async fn list_mcp_resource_templates(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
        server: Option<String>,
        cursor: Option<String>,
    ) -> Result<Value> {
        let broker = self
            .agent_resource_broker(agent, agent_id, cancellation_token)
            .await?;
        broker
            .list_resource_templates(server.as_deref(), cursor)
            .await
    }

    async fn read_mcp_resource(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
        server: String,
        uri: String,
    ) -> Result<Value> {
        let broker = self
            .agent_resource_broker(agent, agent_id, cancellation_token)
            .await?;
        broker.read_resource(&server, &uri).await
    }

    async fn save_task_plan(
        &self,
        agent_id: AgentId,
        title: String,
        markdown: String,
    ) -> Result<TaskSummary> {
        AgentRuntime::save_task_plan(self, agent_id, title, markdown).await
    }

    async fn submit_review_result(
        &self,
        agent_id: AgentId,
        passed: bool,
        findings: String,
        summary: String,
    ) -> Result<TaskReview> {
        AgentRuntime::submit_review_result(self, agent_id, passed, findings, summary).await
    }

    async fn save_artifact(
        &self,
        agent_id: AgentId,
        path: String,
        display_name: Option<String>,
    ) -> Result<ArtifactInfo> {
        AgentRuntime::save_artifact(self, agent_id, path, display_name).await
    }

    async fn execute_project_github_api_get(
        &self,
        agent: &AgentRecord,
        path: String,
    ) -> Result<ToolExecution> {
        AgentRuntime::execute_project_github_api_get(self.as_ref(), agent, &path).await
    }

    async fn execute_mcp_tool(
        &self,
        agent: &AgentRecord,
        model_name: String,
        arguments: Value,
        cancellation_token: CancellationToken,
    ) -> Result<ToolExecution> {
        if agent.summary.read().await.project_id.is_some() {
            return AgentRuntime::execute_project_mcp_tool(
                self.as_ref(),
                agent,
                &model_name,
                arguments,
                cancellation_token,
            )
            .await;
        }
        let manager =
            agent.mcp.read().await.clone().ok_or_else(|| {
                RuntimeError::InvalidInput("MCP manager not initialized".to_string())
            })?;
        let output = tokio::select! {
            output = manager.call_model_tool(&model_name, arguments) => output?,
            _ = cancellation_token.cancelled() => {
                return Err(RuntimeError::TurnCancelled);
            }
        };
        Ok(ToolExecution::new(true, output.to_string(), false))
    }
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

fn is_stale_agent_model_preference_error(err: &RuntimeError) -> bool {
    let RuntimeError::Store(mai_store::StoreError::InvalidConfig(message)) = err else {
        return false;
    };
    (message.starts_with("provider `") && message.ends_with("` not found"))
        || (message.starts_with("model `")
            && message.contains("` is not configured for provider `"))
}

fn project_maintainer_system_prompt(
    owner: &str,
    repo: &str,
    clone_url: &str,
    branch: &str,
) -> String {
    format!(
        r#"You are the Maintainer agent for the GitHub project `{owner}/{repo}`.

The repository clone URL is `{clone_url}`.
You run inside an isolated Docker container. The repository is cloned at `/workspace/repo`; use that path for local inspection and edits.
The selected branch is `{branch}`.

Security rules:
- Do not look for or persist GitHub credentials.
- Do not configure credential helpers.
- Do not write `~/.config/gh`, `~/.git-credentials`, long-lived `GH_TOKEN`, or long-lived `GITHUB_TOKEN`.
- Use MCP/GitHub API tools for GitHub reads and writes such as issues, branches, commits, and pull requests.
- Treat the deployment as no-webhook/no-public-inbound: refresh or poll state when you need current GitHub information.

Operational focus:
- Help the user review, plan, and maintain this repository.
- Prefer small, testable changes.
- Run relevant checks before reporting completion."#
    )
}

fn normalize_optional_path_segment(value: Option<&str>, field: &str) -> Result<Option<String>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.contains(char::is_whitespace)
        || value.starts_with('-')
        || value.starts_with('/')
        || value.contains("..")
        || value.contains('\\')
    {
        return Err(RuntimeError::InvalidInput(format!(
            "{field} must be a safe Git ref name"
        )));
    }
    Ok(Some(value.to_string()))
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn shell_quote(value: &str) -> String {
    shell_words::quote(value).into_owned()
}

fn redact_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        return value.to_string();
    }
    value.replace(secret, "<redacted>")
}

fn runtime_sidecar_image(image: String) -> String {
    let image = image.trim();
    if image.is_empty() {
        DEFAULT_SIDECAR_IMAGE.to_string()
    } else {
        image.to_string()
    }
}

fn safe_artifact_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(RuntimeError::InvalidInput(
            "artifact name cannot be empty".to_string(),
        ));
    }
    if name == "." || name == ".." {
        return Err(RuntimeError::InvalidInput(
            "artifact name must be a file name".to_string(),
        ));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(RuntimeError::InvalidInput(
            "artifact name cannot contain path separators".to_string(),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(RuntimeError::InvalidInput(
            "artifact name cannot contain control characters".to_string(),
        ));
    }
    Ok(name.to_string())
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
        summary.status = AgentStatus::Idle;
        summary.last_error = Some("interrupted by server restart".to_string());
        summary.updated_at = now();
        changed = true;
    }
    (summary, changed)
}

fn short_id(id: AgentId) -> String {
    id.to_string().chars().take(8).collect()
}

#[cfg(test)]
fn extract_skill_mentions(text: &str) -> Vec<String> {
    mai_skills::extract_skill_mentions(text)
}

impl AgentResourceBroker {
    async fn list_resources(&self, server: Option<&str>, cursor: Option<String>) -> Result<Value> {
        if cursor.is_some() && is_skill_resource_server(server) {
            return Ok(json!({
                "server": server,
                "resources": [],
                "nextCursor": null,
            }));
        }
        if is_skill_resource_server(server) {
            return Ok(skill_resources_value(server, &self.skills.skills));
        }
        if let Some(server) = server {
            let normalized = normalize_mcp_resource_server(server);
            if let Some(manager) = self.manager_for_server(&normalized).await {
                return Ok(manager.list_resources(Some(&normalized), cursor).await?);
            }
            return Err(resource_provider_not_found(server));
        }

        let mut resources = Vec::new();
        resources.extend(skill_resource_values(&self.skills.skills));
        for manager in self.mcp_managers() {
            let value = manager.list_resources(None, None).await?;
            if let Some(items) = value.get("resources").and_then(Value::as_array) {
                resources.extend(items.iter().cloned());
            }
        }
        Ok(json!({ "resources": resources }))
    }

    async fn list_resource_templates(
        &self,
        server: Option<&str>,
        cursor: Option<String>,
    ) -> Result<Value> {
        if is_skill_resource_server(server) {
            return Ok(json!({
                "server": server,
                "resourceTemplates": [],
                "nextCursor": null,
            }));
        }
        if let Some(server) = server {
            let normalized = normalize_mcp_resource_server(server);
            if let Some(manager) = self.manager_for_server(&normalized).await {
                return Ok(manager
                    .list_resource_templates(Some(&normalized), cursor)
                    .await?);
            }
            return Err(resource_provider_not_found(server));
        }

        let mut templates = Vec::new();
        for manager in self.mcp_managers() {
            let value = manager.list_resource_templates(None, None).await?;
            if let Some(items) = value.get("resourceTemplates").and_then(Value::as_array) {
                templates.extend(items.iter().cloned());
            }
        }
        Ok(json!({ "resourceTemplates": templates }))
    }

    async fn read_resource(&self, server: &str, uri: &str) -> Result<Value> {
        if is_skill_resource_server(Some(server)) || uri.starts_with(SKILL_RESOURCE_SCHEME) {
            return self.read_skill_resource(uri);
        }
        let normalized = normalize_mcp_resource_server(server);
        if let Some(manager) = self.manager_for_server(&normalized).await {
            return Ok(manager.read_resource(&normalized, uri).await?);
        }
        Err(resource_provider_not_found(server))
    }

    fn read_skill_resource(&self, uri: &str) -> Result<Value> {
        let Some(resource) = uri.strip_prefix(SKILL_RESOURCE_SCHEME) else {
            return Err(RuntimeError::InvalidInput(format!(
                "invalid skill resource uri `{uri}`; expected skill:///<skill-name>"
            )));
        };
        let resource = resource.trim_start_matches('/');
        let (name, relative) = resource.split_once('/').unwrap_or((resource, ""));
        let name = name.trim();
        if name.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "skill resource uri must include a skill name".to_string(),
            ));
        }
        let matches = self
            .skills
            .skills
            .iter()
            .filter(|skill| skill.enabled && skill.name == name)
            .collect::<Vec<_>>();
        let skill = match matches.as_slice() {
            [skill] => *skill,
            [] => {
                return Err(RuntimeError::InvalidInput(format!(
                    "skill resource not found: {uri}"
                )));
            }
            _ => {
                return Err(RuntimeError::InvalidInput(format!(
                    "ambiguous skill resource `{name}`; select a specific skill path"
                )));
            }
        };
        let path = if relative.is_empty() {
            skill.path.clone()
        } else {
            let relative = safe_skill_resource_relative_path(relative)?;
            let Some(skill_dir) = skill.path.parent() else {
                return Err(RuntimeError::InvalidInput(format!(
                    "skill resource has no parent directory: {uri}"
                )));
            };
            skill_dir.join(relative)
        };
        let contents = fs::read_to_string(&path)?;
        Ok(json!({
            "contents": [{
                "uri": uri,
                "mimeType": skill_resource_mime_type(&path),
                "text": contents,
            }]
        }))
    }

    async fn manager_for_server(&self, server: &str) -> Option<Arc<McpAgentManager>> {
        for manager in self.mcp_managers() {
            if manager
                .resource_servers()
                .await
                .iter()
                .any(|resource_server| resource_server == server)
            {
                return Some(manager);
            }
        }
        None
    }

    fn mcp_managers(&self) -> Vec<Arc<McpAgentManager>> {
        let mut managers = Vec::new();
        if let Some(manager) = &self.agent_mcp {
            managers.push(Arc::clone(manager));
        }
        if let Some(manager) = &self.project_mcp
            && !managers
                .iter()
                .any(|existing| Arc::ptr_eq(existing, manager))
        {
            managers.push(Arc::clone(manager));
        }
        managers
    }
}

fn skill_resources_value(server: Option<&str>, skills: &[mai_protocol::SkillMetadata]) -> Value {
    json!({
        "server": server,
        "resources": skill_resource_values(skills),
        "nextCursor": null,
    })
}

fn skill_resource_values(skills: &[mai_protocol::SkillMetadata]) -> Vec<Value> {
    skills
        .iter()
        .filter(|skill| skill.enabled)
        .map(|skill| {
            json!({
                "server": skill_resource_server_for_scope(skill.scope),
                "uri": skill_uri(&skill.name),
                "name": skill.name,
                "description": skill.description,
                "mimeType": "text/markdown",
            })
        })
        .collect()
}

fn skill_resource_server_for_scope(scope: SkillScope) -> &'static str {
    if scope == SkillScope::Project {
        PROJECT_SKILL_RESOURCE_SERVER
    } else {
        SKILL_RESOURCE_SERVER
    }
}

fn skill_uri(name: &str) -> String {
    format!("{SKILL_RESOURCE_SCHEME}{name}")
}

fn safe_skill_resource_relative_path(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::CurDir => {}
            _ => {
                return Err(RuntimeError::InvalidInput(
                    "skill resource relative path cannot be absolute or contain parent components"
                        .to_string(),
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "skill resource relative path cannot be empty".to_string(),
        ));
    }
    Ok(normalized)
}

fn skill_resource_mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("md") => "text/markdown",
        Some("py") => "text/x-python",
        Some("sh") => "text/x-shellscript",
        Some("json") => "application/json",
        Some("yaml" | "yml") => "application/yaml",
        _ => "text/plain",
    }
}

fn is_skill_resource_server(server: Option<&str>) -> bool {
    server.is_some_and(|server| {
        server == SKILL_RESOURCE_SERVER
            || server == PROJECT_SKILL_RESOURCE_SERVER
            || server == format!("mcp:{SKILL_RESOURCE_SERVER}")
            || server == format!("mcp:{PROJECT_SKILL_RESOURCE_SERVER}")
    })
}

fn normalize_mcp_resource_server(server: &str) -> Cow<'_, str> {
    server
        .strip_prefix("mcp:")
        .map(Cow::Borrowed)
        .unwrap_or_else(|| Cow::Borrowed(server))
}

fn resource_provider_not_found(server: &str) -> RuntimeError {
    RuntimeError::InvalidInput(format!("resource provider not found: {server}"))
}

fn should_auto_compact(last_context_tokens: u64, context_tokens: u64) -> bool {
    if last_context_tokens == 0 || context_tokens == 0 {
        return false;
    }
    last_context_tokens.saturating_mul(100)
        >= context_tokens.saturating_mul(AUTO_COMPACT_THRESHOLD_PERCENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{
        GitProvider, ModelConfig, ModelReasoningConfig, ModelReasoningVariant, ProviderConfig,
        ProviderKind, ProvidersConfigRequest,
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
            wire_api: Default::default(),
            capabilities: Default::default(),
            request_policy: Default::default(),
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
                wire_api: mai_protocol::ModelWireApi::ChatCompletions,
                capabilities: Default::default(),
                request_policy: Default::default(),
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
                    let is_model_request = request
                        .get("request_line")
                        .and_then(Value::as_str)
                        .is_some_and(|line| line.contains(" /responses "))
                        || request.get("model").is_some();
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
                    if let Some(delay_ms) = response.get("__delay_ms").and_then(Value::as_u64) {
                        sleep(Duration::from_millis(delay_ms)).await;
                    }
                    write_mock_response(&mut stream, response, is_model_request).await;
                });
            }
        });
        (format!("http://{addr}"), requests)
    }

    async fn wait_until<F, Fut>(mut condition: F, timeout: Duration)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = Instant::now() + timeout;
        loop {
            if condition().await {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for condition");
            sleep(Duration::from_millis(20)).await;
        }
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
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let content_length = content_length(&headers);
        while buffer.len() < header_end + content_length {
            let read = stream.read(&mut chunk).await.expect("read request body");
            assert!(read > 0, "mock request closed before body");
            buffer.extend_from_slice(&chunk[..read]);
        }
        if content_length == 0 {
            let request_line = headers.lines().next().unwrap_or_default();
            return json!({ "request_line": request_line });
        }
        let request_line = headers.lines().next().unwrap_or_default();
        let mut value: Value =
            serde_json::from_slice(&buffer[header_end..header_end + content_length])
                .expect("request json");
        if let Some(object) = value.as_object_mut() {
            object.insert("request_line".to_string(), json!(request_line));
        }
        value
    }

    async fn write_mock_response(
        stream: &mut tokio::net::TcpStream,
        response: Value,
        is_model_request: bool,
    ) {
        let status = response
            .get("__status")
            .and_then(Value::as_u64)
            .unwrap_or(200);
        let headers = response
            .get("__headers")
            .and_then(Value::as_object)
            .cloned();
        let mut body_value = response;
        if let Some(object) = body_value.as_object_mut() {
            object.remove("__status");
            object.remove("__headers");
        }
        let body = if status == 200 && is_model_request {
            mock_sse_body(&body_value)
        } else {
            serde_json::to_string(&body_value).expect("response json")
        };
        let reason = if status == 200 { "OK" } else { "ERROR" };
        let extra_headers = headers
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(name, value)| value.as_str().map(|value| format!("{name}: {value}\r\n")))
            .collect::<String>();
        let content_type = if status == 200 && is_model_request {
            "text/event-stream"
        } else {
            "application/json"
        };
        let reply = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(reply.as_bytes())
            .await
            .expect("write response");
    }

    fn mock_sse_body(response: &Value) -> String {
        let response_id = response
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("resp_mock");
        let mut events = vec![json!({
            "type": "response.created",
            "response": { "id": response_id }
        })];
        for (index, item) in response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            events.push(json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": item,
            }));
        }
        events.push(json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "usage": response.get("usage").cloned().unwrap_or(Value::Null),
            }
        }));
        events
            .into_iter()
            .map(|event| {
                let kind = event
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message");
                format!("event: {kind}\ndata: {event}\n\n")
            })
            .collect()
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

    fn compact_no_continuation_test_provider(base_url: String) -> ProviderConfig {
        let mut provider = compact_test_provider(base_url);
        provider.models[0].capabilities.continuation = false;
        provider
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
            project_id: None,
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
            ConfigStore::open_with_config_and_artifact_index_path(
                &dir.path().join("runtime.sqlite3"),
                &dir.path().join("config.toml"),
                &dir.path().join("data/artifacts/index"),
            )
            .await
            .expect("open store"),
        )
    }

    async fn save_agent_with_session(store: &ConfigStore, summary: &AgentSummary) {
        store.save_agent(summary, None).await.expect("save agent");
        save_test_session(store, summary.id, Uuid::new_v4()).await;
    }

    fn write_skill_at(base: PathBuf, name: &str, description: &str, body: &str) -> PathBuf {
        let skill_dir = base.join(name);
        fs::create_dir_all(&skill_dir).expect("mkdir skill");
        let path = skill_dir.join("SKILL.md");
        fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
        )
        .expect("write skill");
        path
    }

    fn write_project_skill(
        runtime: &AgentRuntime,
        project_id: ProjectId,
        name: &str,
        description: &str,
        body: &str,
    ) -> PathBuf {
        write_skill_at(
            runtime.project_skill_cache_dir(project_id).join("claude"),
            name,
            description,
            body,
        )
    }

    fn write_workspace_project_skill(
        dir: &tempfile::TempDir,
        root: &str,
        name: &str,
        description: &str,
        body: &str,
    ) -> PathBuf {
        write_skill_at(
            fake_sidecar_workspace_path(dir).join(root),
            name,
            description,
            body,
        )
    }

    async fn test_runtime(dir: &tempfile::TempDir, store: Arc<ConfigStore>) -> Arc<AgentRuntime> {
        AgentRuntime::new(
            DockerClient::new_with_binary("unused", fake_docker_path(dir)),
            ModelClient::new(),
            store,
            test_runtime_config(dir, DEFAULT_SIDECAR_IMAGE),
        )
        .await
        .expect("runtime")
    }

    async fn test_runtime_with_sidecar_image_and_git(
        dir: &tempfile::TempDir,
        store: Arc<ConfigStore>,
        sidecar_image: &str,
    ) -> Arc<AgentRuntime> {
        AgentRuntime::new(
            DockerClient::new_with_binary("unused-agent", fake_docker_path(dir)),
            ModelClient::new(),
            store,
            RuntimeConfig {
                git_binary: Some(fake_git_path(dir)),
                ..test_runtime_config(dir, sidecar_image)
            },
        )
        .await
        .expect("runtime")
    }

    async fn test_runtime_with_github_api(
        dir: &tempfile::TempDir,
        store: Arc<ConfigStore>,
        github_api_base_url: String,
    ) -> Arc<AgentRuntime> {
        AgentRuntime::new(
            DockerClient::new_with_binary("unused", fake_docker_path(dir)),
            ModelClient::new(),
            store,
            RuntimeConfig {
                github_api_base_url: Some(github_api_base_url),
                ..test_runtime_config(dir, DEFAULT_SIDECAR_IMAGE)
            },
        )
        .await
        .expect("runtime")
    }

    fn test_runtime_config(dir: &tempfile::TempDir, sidecar_image: &str) -> RuntimeConfig {
        RuntimeConfig {
            repo_root: dir.path().to_path_buf(),
            cache_root: dir.path().join("cache"),
            artifact_files_root: dir.path().join("data/artifacts/files"),
            sidecar_image: sidecar_image.to_string(),
            github_api_base_url: None,
            git_binary: None,
            system_skills_root: None,
            system_agents_root: None,
        }
    }

    fn test_project_summary(
        project_id: ProjectId,
        maintainer_agent_id: AgentId,
        git_account_id: &str,
    ) -> ProjectSummary {
        let timestamp = now();
        ProjectSummary {
            id: project_id,
            name: "owner/repo".to_string(),
            status: ProjectStatus::Creating,
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_full_name: "owner/repo".to_string(),
            git_account_id: Some(git_account_id.to_string()),
            repository_id: 42,
            installation_id: 0,
            installation_account: "owner".to_string(),
            branch: "main".to_string(),
            docker_image: "unused-agent".to_string(),
            clone_status: ProjectCloneStatus::Pending,
            maintainer_agent_id,
            created_at: timestamp,
            updated_at: timestamp,
            last_error: None,
            auto_review_enabled: false,
            reviewer_extra_prompt: None,
            review_status: ProjectReviewStatus::Disabled,
            current_reviewer_agent_id: None,
            last_review_started_at: None,
            last_review_finished_at: None,
            next_review_at: None,
            last_review_outcome: None,
            review_last_error: None,
        }
    }

    fn ready_test_project_summary(
        project_id: ProjectId,
        maintainer_agent_id: AgentId,
        git_account_id: &str,
    ) -> ProjectSummary {
        let mut summary = test_project_summary(project_id, maintainer_agent_id, git_account_id);
        summary.status = ProjectStatus::Ready;
        summary.clone_status = ProjectCloneStatus::Ready;
        summary
    }

    fn test_mcp_tool(server: &str, name: &str) -> McpTool {
        McpTool {
            server: server.to_string(),
            name: name.to_string(),
            model_name: mai_mcp::model_tool_name(server, name),
            description: format!("{server} {name}"),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
        }
    }

    fn fake_docker_path(dir: &tempfile::TempDir) -> String {
        let path = dir.path().join("fake-docker.sh");
        let log_path = fake_docker_log_path(dir);
        let workspace_root = dir.path().join("fake-sidecar-workspace");
        let script = format!(
            r#"#!/bin/sh
	LOG={}
	WORKSPACE={}
	last_created="created-container"
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
	    if printf '%s' "$*" | grep -q 'mai-review-skill-copy'; then
	      echo "review-copy-container"
	    else
	      echo "created-container"
	    fi
	    exit 0
	    ;;
	  run)
	    echo "$*" >> "$LOG"
	    command=""
	    last=""
	    for arg in "$@"; do
	      if [ "$last" = "-lc" ]; then
	        command="$arg"
	      fi
	      last="$arg"
	    done
	    if printf '%s' "$command" | grep -q "/workspace/repo/.claude/skills"; then
	      [ -d "$WORKSPACE/.claude/skills" ] && printf '%s\t%s\t%s\n' ".claude/skills" "claude" "/workspace/repo/.claude/skills"
	      [ -d "$WORKSPACE/.agents/skills" ] && printf '%s\t%s\t%s\n' ".agents/skills" "agents" "/workspace/repo/.agents/skills"
	      [ -d "$WORKSPACE/skills" ] && printf '%s\t%s\t%s\n' "skills" "skills" "/workspace/repo/skills"
	    fi
	    if printf '%s' "$command" | grep -q "fetch --prune origin"; then
	      echo "review-sync" >> "$LOG"
	    fi
	    exit 0
	    ;;
  rm|rmi|start)
    echo "$*" >> "$LOG"
    exit 0
    ;;
  exec)
    echo "$*" >> "$LOG"
    command=""
    last=""
    for arg in "$@"; do
      if [ "$last" = "-lc" ]; then
        command="$arg"
      fi
      last="$arg"
    done
	    if printf '%s' "$command" | grep -q "/workspace/repo/.claude/skills"; then
	      [ -d "$WORKSPACE/.claude/skills" ] && printf '%s\t%s\t%s\n' ".claude/skills" "claude" "/workspace/repo/.claude/skills"
	      [ -d "$WORKSPACE/.agents/skills" ] && printf '%s\t%s\t%s\n' ".agents/skills" "agents" "/workspace/repo/.agents/skills"
	      [ -d "$WORKSPACE/skills" ] && printf '%s\t%s\t%s\n' "skills" "skills" "/workspace/repo/skills"
	    fi
	    if printf '%s' "$command" | grep -q "git -c credential.helper= clone"; then
	      echo "sidecar-git-clone" >> "$LOG"
	      if [ -n "$MAI_GITHUB_INSTALLATION_TOKEN" ]; then
	        echo "token-present" >> "$LOG"
	      fi
	      mkdir -p "$WORKSPACE"
	      printf 'hello\n' > "$WORKSPACE/README.md"
	    fi
	    command=$(printf '%s' "$command" | sed "s#/workspace/repo#$WORKSPACE#g")
	    if printf '%s' "$command" | grep -q "sed -n"; then
	      /bin/sh -lc "$command"
	    elif printf '%s' "$command" | grep -q "dd if="; then
	      /bin/sh -lc "$command"
	    elif printf '%s' "$command" | grep -q "^find "; then
	      /bin/sh -lc "$command"
	    elif printf '%s' "$command" | grep -q "rg --files"; then
	      /bin/sh -lc "$command"
	    elif printf '%s' "$command" | grep -q "rg --json"; then
	      /bin/sh -lc "$command"
	    elif printf '%s' "$command" | grep -q "printf /workspace"; then
	      /bin/sh -lc "$command"
	    elif printf '%s' "$command" | grep -q "rm -f"; then
	      /bin/sh -lc "$command"
	    elif printf '%s' "$command" | grep -q "test -f"; then
	      /bin/sh -lc "$command"
	    fi
	    exit 0
	    ;;
    cp)
    echo "$*" >> "$LOG"
	    container="${{2%%:*}}"
	    if [ "$2" = "${{container}}:/workspace/repo/.claude/skills" ]; then
	      rm -rf "$3"
	      cp -R "$WORKSPACE/.claude/skills" "$3"
	    elif [ "$2" = "${{container}}:/workspace/repo/.agents/skills" ]; then
	      rm -rf "$3"
	      cp -R "$WORKSPACE/.agents/skills" "$3"
	    elif [ "$2" = "${{container}}:/workspace/repo/skills" ]; then
	      rm -rf "$3"
	      cp -R "$WORKSPACE/skills" "$3"
    elif printf '%s' "$3" | grep -q ':/workspace/.mai-team/skills'; then
      :
    elif printf '%s' "$3" | grep -q '^created-container:'; then
      dest="${{3#created-container:}}"
      dest=$(printf '%s' "$dest" | sed "s#/workspace/repo#$WORKSPACE#g")
      mkdir -p "$(dirname "$dest")"
      cp "$2" "$dest"
    elif printf '%s' "$2" | grep -q '^created-container:'; then
      src="${{2#created-container:}}"
      src=$(printf '%s' "$src" | sed "s#/workspace/repo#$WORKSPACE#g")
      mkdir -p "$(dirname "$3")"
      if [ -e "$src" ]; then
        cp "$src" "$3"
      else
        printf 'artifact\n' > "$3"
      fi
    fi
    exit 0
    ;;
  *)
    echo "$*" >> "$LOG"
    exit 0
    ;;
esac
"#,
            test_shell_quote(&log_path.to_string_lossy()),
            test_shell_quote(&workspace_root.to_string_lossy())
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

    fn fake_sidecar_workspace_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("fake-sidecar-workspace")
    }

    fn fake_git_path(dir: &tempfile::TempDir) -> String {
        let path = dir.path().join("fake-git.sh");
        let log_path = fake_git_log_path(dir);
        let script = format!(
            r#"#!/bin/sh
LOG={}
echo "$*" >> "$LOG"
if [ -n "$GIT_ASKPASS" ]; then
  echo "askpass=$GIT_ASKPASS" >> "$LOG"
fi
if [ -n "$MAI_GITHUB_INSTALLATION_TOKEN" ]; then
  echo "token-present" >> "$LOG"
fi
last=""
for arg in "$@"; do
  last="$arg"
done
mkdir -p "$last"
printf 'hello\n' > "$last/README.md"
exit 0
"#,
            test_shell_quote(&log_path.to_string_lossy())
        );
        std::fs::write(&path, script).expect("write fake git");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path)
                .expect("fake git metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).expect("chmod fake git");
        }
        path.to_string_lossy().to_string()
    }

    fn failing_docker_path(dir: &tempfile::TempDir) -> String {
        let path = dir.path().join("failing-docker.sh");
        let log_path = fake_docker_log_path(dir);
        let script = format!(
            r#"#!/bin/sh
LOG={}
echo "$*" >> "$LOG"
case "$1" in
  create)
    echo "container startup failed" >&2
    exit 42
    ;;
  ps|rm|rmi|start|exec|commit)
    exit 0
    ;;
  *)
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
            std::fs::set_permissions(&path, permissions).expect("chmod docker");
        }
        path.to_string_lossy().to_string()
    }

    fn fake_docker_log_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("fake-docker.log")
    }

    fn fake_git_log_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("fake-git.log")
    }

    fn fake_docker_log(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(fake_docker_log_path(dir)).unwrap_or_default()
    }

    fn fake_git_log(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(fake_git_log_path(dir)).unwrap_or_default()
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

    #[tokio::test]
    async fn save_git_account_returns_verifying_without_waiting_for_verify() {
        let (base_url, _requests) = start_mock_responses(vec![json!({
            "__close_without_response": true
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let runtime = test_runtime_with_github_api(&dir, Arc::clone(&store), base_url).await;

        let response = runtime
            .save_git_account(GitAccountRequest {
                id: Some("account-1".to_string()),
                provider: GitProvider::Github,
                label: "Personal".to_string(),
                token: Some("secret-token".to_string()),
                is_default: true,
                ..Default::default()
            })
            .await
            .expect("save account");

        assert_eq!(response.account.status, GitAccountStatus::Verifying);
        assert_eq!(response.account.last_error, None);
    }

    #[tokio::test]
    async fn verify_git_account_records_success_metadata() {
        let (base_url, _requests) = start_mock_responses(vec![json!({
            "__headers": {
                "x-oauth-scopes": "repo, read:packages"
            },
            "login": "octo"
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .upsert_git_account(GitAccountRequest {
                id: Some("account-1".to_string()),
                provider: GitProvider::Github,
                label: "Personal".to_string(),
                token: Some("ghp_secret".to_string()),
                is_default: true,
                ..Default::default()
            })
            .await
            .expect("save account");
        let runtime = test_runtime_with_github_api(&dir, Arc::clone(&store), base_url).await;

        let account = runtime
            .verify_git_account("account-1")
            .await
            .expect("verify account");

        assert_eq!(account.status, GitAccountStatus::Verified);
        assert_eq!(account.login.as_deref(), Some("octo"));
        assert_eq!(account.token_kind, GitTokenKind::Classic);
        assert!(account.scopes.contains(&"repo".to_string()));
        assert!(account.last_verified_at.is_some());
        assert_eq!(account.last_error, None);
    }

    #[tokio::test]
    async fn verify_git_account_records_failed_http_error() {
        let (base_url, _requests) = start_mock_responses(vec![json!({
            "__status": 401,
            "message": "Bad credentials"
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .upsert_git_account(GitAccountRequest {
                id: Some("account-1".to_string()),
                provider: GitProvider::Github,
                label: "Personal".to_string(),
                token: Some("secret-token".to_string()),
                is_default: true,
                ..Default::default()
            })
            .await
            .expect("save account");
        let runtime = test_runtime_with_github_api(&dir, Arc::clone(&store), base_url).await;

        let account = runtime
            .verify_git_account("account-1")
            .await
            .expect("verify account");

        assert_eq!(account.status, GitAccountStatus::Failed);
        assert!(account.last_verified_at.is_some());
        assert!(
            account
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("Bad credentials")
        );
    }

    #[test]
    fn extracts_skill_mentions() {
        assert_eq!(
            extract_skill_mentions("please use $rust-dev, then $plugin:doc and $PATH."),
            vec!["rust-dev", "plugin:doc"]
        );
    }

    #[test]
    fn project_review_sync_command_fetches_pr_refs_without_token_literal() {
        let command = projects::review::review_repo_sync_command(
            "https://github.com/owner/repo.git",
            "https://github.com/owner/repo.git",
            "main",
        );
        assert!(command.contains("'+refs/pull/*/head:refs/remotes/origin/pr/*'"));
        assert!(command.contains("--no-tags origin +refs/heads/main:refs/remotes/origin/main"));
        assert!(command.contains("--no-tags origin '+refs/pull/*/head:refs/remotes/origin/pr/*'"));
        assert!(command.contains("-c http.version=HTTP/1.1"));
        assert!(command.contains("-c http.lowSpeedLimit=1 -c http.lowSpeedTime=300"));
        assert!(command.contains("git worktree prune"));
        assert!(command.contains("git reset --hard HEAD"));
        assert!(command.contains("git clean -fdx"));
        assert!(command.contains("git checkout -B main origin/main"));
        assert!(command.contains("git reset --hard origin/main"));
        assert!(command.contains("MAI_GITHUB_REVIEW_TOKEN"));
        assert!(!command.contains("ghp_"));
    }

    #[test]
    fn project_review_reclone_command_removes_stale_repo_before_ensure() {
        let command = projects::review::review_repo_reclone_command(
            "https://github.com/owner/repo.git",
            "https://github.com/owner/repo.git",
            "main",
        );
        assert!(command.contains("rm -rf /workspace/repo"));
        assert!(command.contains("clone --branch main"));
        assert!(command.contains("--no-tags origin '+refs/pull/*/head:refs/remotes/origin/pr/*'"));
        assert!(command.contains("mkdir -p /workspace/reviews"));
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
                providers: vec![compact_test_provider("http://localhost".to_string())],
                default_provider_id: Some("mock".to_string()),
            })
            .await
            .expect("save providers");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        let task = runtime
            .create_task(
                Some("Build task UI".to_string()),
                None,
                Some("ubuntu:latest".to_string()),
            )
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
        assert!(
            runtime
                .create_session(detail.selected_agent.summary.id)
                .await
                .is_err()
        );

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
            .create_task(
                Some("Plan me".to_string()),
                None,
                Some("ubuntu:latest".to_string()),
            )
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
    async fn update_todo_list_accepts_todos_json_string_alias() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let agent_id = Uuid::new_v4();
        save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container-1"))).await;
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        let output = runtime
            .execute_tool_for_test(
                agent_id,
                "update_todo_list",
                json!({
                    "todos": r#"[{"step":"获取认证用户信息和读取 helper 脚本","status":"in_progress"},{"step":"选择一个符合条件的 PR","status":"pending"}]"#
                }),
            )
            .await
            .expect("update todo list");

        assert!(output.success);
        let events = runtime.events.snapshot().await;
        let items = events
            .iter()
            .rev()
            .find_map(|event| match &event.kind {
                ServiceEventKind::TodoListUpdated {
                    agent_id: event_agent_id,
                    items,
                    ..
                } if *event_agent_id == agent_id => Some(items),
                _ => None,
            })
            .expect("todo event");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].step, "获取认证用户信息和读取 helper 脚本");
        assert_eq!(items[0].status, mai_protocol::TodoListStatus::InProgress);
    }

    #[tokio::test]
    async fn read_file_returns_bounded_paged_output() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let agent_id = Uuid::new_v4();
        save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container-1"))).await;
        let workspace = fake_sidecar_workspace_path(&dir);
        fs::create_dir_all(&workspace).expect("mkdir workspace");
        fs::write(workspace.join("sample.txt"), "alpha\nbeta\ngamma\n").expect("write file");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        let output = runtime
            .execute_tool_for_test(
                agent_id,
                "read_file",
                json!({
                    "path": "/workspace/repo/sample.txt",
                    "line_start": 2,
                    "line_count": 2,
                    "max_bytes": 20
                }),
            )
            .await
            .expect("read file");

        assert!(output.success);
        let value = serde_json::from_str::<Value>(&output.output).expect("json output");
        assert_eq!(value["text"], "beta\ngamma\n");
        assert_eq!(value["truncated"], false);
    }

    #[tokio::test]
    async fn file_tools_list_files_respects_glob_and_limit() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let agent_id = Uuid::new_v4();
        save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container-1"))).await;
        let workspace = fake_sidecar_workspace_path(&dir);
        fs::create_dir_all(workspace.join("src")).expect("mkdir workspace");
        fs::write(workspace.join("src/a.rs"), "fn a() {}\n").expect("write file");
        fs::write(workspace.join("src/b.rs"), "fn b() {}\n").expect("write file");
        fs::write(workspace.join("README.md"), "hello\n").expect("write file");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        let output = runtime
            .execute_tool_for_test(
                agent_id,
                "list_files",
                json!({
                    "path": "/workspace/repo",
                    "glob": "*.rs",
                    "max_files": 1
                }),
            )
            .await
            .expect("list files");

        assert!(output.success);
        let value = serde_json::from_str::<Value>(&output.output).expect("json output");
        assert_eq!(value["count"], 1);
        assert_eq!(value["truncated"], true);
        assert!(
            value["files"]
                .as_array()
                .expect("files")
                .iter()
                .all(|path| path.as_str().is_some_and(|path| path.ends_with(".rs")))
        );
    }

    #[tokio::test]
    async fn file_tools_search_files_returns_structured_matches() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let agent_id = Uuid::new_v4();
        save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container-1"))).await;
        let workspace = fake_sidecar_workspace_path(&dir);
        fs::create_dir_all(&workspace).expect("mkdir workspace");
        fs::write(workspace.join("one.txt"), "alpha\nbeta\n").expect("write file");
        fs::write(workspace.join("two.md"), "ALPHA\n").expect("write file");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        let output = runtime
            .execute_tool_for_test(
                agent_id,
                "search_files",
                json!({
                    "query": "alpha",
                    "path": "/workspace/repo",
                    "glob": "*.txt",
                    "literal": true,
                    "max_matches": 5
                }),
            )
            .await
            .expect("search files");

        assert!(output.success);
        let value = serde_json::from_str::<Value>(&output.output).expect("json output");
        assert_eq!(value["count"], 1);
        assert_eq!(value["matches"][0]["line"], 1);
        assert!(
            value["matches"][0]["text"]
                .as_str()
                .unwrap()
                .contains("alpha")
        );
    }

    #[tokio::test]
    async fn file_tools_apply_patch_add_update_delete_and_move() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let agent_id = Uuid::new_v4();
        save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container-1"))).await;
        let workspace = fake_sidecar_workspace_path(&dir);
        fs::create_dir_all(&workspace).expect("mkdir workspace");
        fs::write(workspace.join("edit.txt"), "one\ntwo\nthree\n").expect("write file");
        fs::write(workspace.join("delete.txt"), "remove me\n").expect("write file");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let patch = "*** Begin Patch\n*** Add File: added.txt\n+created\n*** Update File: edit.txt\n*** Move to: moved.txt\n@@\n one\n-two\n+dos\n three\n*** Delete File: delete.txt\n*** End Patch\n";

        let output = runtime
            .execute_tool_for_test(
                agent_id,
                "apply_patch",
                json!({
                    "cwd": "/workspace/repo",
                    "input": patch
                }),
            )
            .await
            .expect("apply patch");

        assert!(output.success);
        assert_eq!(
            fs::read_to_string(workspace.join("added.txt")).expect("added"),
            "created\n"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("moved.txt")).expect("moved"),
            "one\ndos\nthree\n"
        );
        assert!(!workspace.join("edit.txt").exists());
        assert!(!workspace.join("delete.txt").exists());
        let value = serde_json::from_str::<Value>(&output.output).expect("json output");
        assert!(value["changed_files"].as_array().unwrap().len() >= 3);
    }

    #[tokio::test]
    async fn file_tools_apply_patch_rejects_bad_paths_and_mismatched_hunks() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let agent_id = Uuid::new_v4();
        save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container-1"))).await;
        let workspace = fake_sidecar_workspace_path(&dir);
        fs::create_dir_all(&workspace).expect("mkdir workspace");
        fs::write(workspace.join("edit.txt"), "one\n").expect("write file");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        let bad_path = runtime
            .execute_tool_for_test(
                agent_id,
                "apply_patch",
                json!({
                    "cwd": "/workspace/repo",
                    "input": "*** Begin Patch\n*** Add File: ../bad.txt\n+nope\n*** End Patch\n"
                }),
            )
            .await;
        assert!(bad_path.is_err());

        let mismatch = runtime
            .execute_tool_for_test(
                agent_id,
                "apply_patch",
                json!({
                    "cwd": "/workspace/repo",
                    "input": "*** Begin Patch\n*** Update File: edit.txt\n@@\n-two\n+dos\n*** End Patch\n"
                }),
            )
            .await;
        assert!(mismatch.is_err());
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
            .create_task(
                Some("Needs plan".to_string()),
                None,
                Some("ubuntu:latest".to_string()),
            )
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
            agents::descendant_delete_order_from_summaries(parent, &summaries),
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
        let events = runtime.events.snapshot().await;
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
        assert!(!should_auto_compact(89, 100));
        assert!(should_auto_compact(90, 100));
        assert!(should_auto_compact(360_000, 400_000));
        assert!(!should_auto_compact(90, 0));
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
            turn::history::compact_summary_from_output(&output).as_deref(),
            Some("second")
        );
        assert_eq!(turn::history::compact_summary_from_output(&[]), None);
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
        turn::history::repair_incomplete_tool_history(&mut history);
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
        turn::history::repair_incomplete_tool_history(&mut history);
        assert_eq!(history.len(), 3);
        assert!(matches!(
            &history[2],
            ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_2"
        ));
    }

    #[test]
    fn repair_adds_missing_tool_outputs_for_function_call() {
        let mut history = vec![ModelInputItem::FunctionCall {
            call_id: "call_a".to_string(),
            name: "container_exec".to_string(),
            arguments: "{}".to_string(),
        }];
        turn::history::repair_incomplete_tool_history(&mut history);
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
        turn::history::repair_incomplete_tool_history(&mut history);
        assert_eq!(history.len(), 4);
    }

    #[test]
    fn repair_does_nothing_for_empty_history() {
        let mut history: Vec<ModelInputItem> = vec![];
        turn::history::repair_incomplete_tool_history(&mut history);
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
        turn::history::repair_incomplete_tool_history(&mut history);
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
        turn::history::repair_incomplete_tool_history(&mut history);
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
            ModelInputItem::user_text(turn::history::compact_summary_message(
                "old summary",
                COMPACT_SUMMARY_PREFIX,
            )),
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

        let compacted = turn::history::build_compacted_history(
            &history,
            "new summary",
            COMPACT_USER_MESSAGE_MAX_CHARS,
            COMPACT_SUMMARY_PREFIX,
        );
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
                if matches!(&content[0], ModelContentItem::InputText { text } if text.contains("new summary") && turn::history::is_compact_summary(text, COMPACT_SUMMARY_PREFIX))
        ));
    }

    #[test]
    fn recent_user_messages_truncates_from_oldest_side() {
        let history = vec![
            ModelInputItem::user_text("abcdef"),
            ModelInputItem::user_text("ghij"),
        ];

        assert_eq!(
            turn::history::recent_user_messages(&history, 7, COMPACT_SUMMARY_PREFIX),
            vec!["def", "ghij"]
        );
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
            project_id: None,
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
            ModelClient::new(),
            store,
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
        )
        .await
        .expect("runtime");

        let agents = runtime.list_agents().await;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].status, AgentStatus::Idle);
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
            .events
            .publish(ServiceEventKind::AgentStatusChanged {
                agent_id,
                status: AgentStatus::Failed,
            })
            .await;
        let events = runtime.events.snapshot().await;
        assert_eq!(events.last().expect("event").sequence, 42);
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
            project_id: None,
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
        let completed = value["completed"].as_array().expect("completed");
        assert_eq!(completed.len(), 1);
        let child_output = &completed[0];
        assert_eq!(
            child_output["final_response"].as_str(),
            Some("Explorer conclusion: auth lives in crates/auth.")
        );
        assert_eq!(
            child_output["recent_messages"]
                .as_array()
                .expect("messages")
                .len(),
            2
        );
        assert_eq!(
            child_output["agent"]["id"].as_str(),
            Some(child_id.to_string().as_str())
        );
        assert_eq!(value["timed_out"].as_bool(), Some(false));
        assert!(matches!(
            runtime.agent(child_id).await,
            Err(RuntimeError::AgentNotFound(id)) if id == child_id
        ));
        assert!(
            runtime
                .list_agents()
                .await
                .iter()
                .all(|agent| agent.id != child_id)
        );
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
            project_id: None,
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
            ModelClient::new(),
            Arc::new(
                ConfigStore::open_with_config_path(&db_path, &config_path)
                    .await
                    .expect("reopen store"),
            ),
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
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
        assert_eq!(trace.agent_id, agent_id);
        assert_eq!(trace.session_id, Some(session_id));
        assert!(trace.output_preview.contains("\"stdout\": \"hello\""));
    }

    #[tokio::test]
    async fn tool_trace_prefers_persisted_trace_records() {
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
        let summary = test_agent_summary(agent_id, None);
        store.save_agent(&summary, None).await.expect("save agent");
        save_test_session(&store, agent_id, session_id).await;
        store
            .save_tool_trace_completed(
                &ToolTraceDetail {
                    agent_id,
                    session_id: Some(session_id),
                    turn_id: Some(turn_id),
                    call_id: "call_persisted".to_string(),
                    tool_name: "container_exec".to_string(),
                    arguments: json!({ "command": "printf persisted" }),
                    output: r#"{"status":0,"stdout":"persisted","stderr":""}"#.to_string(),
                    success: true,
                    duration_ms: Some(99),
                    started_at: Some(timestamp),
                    completed_at: Some(timestamp),
                    output_preview: "persisted".to_string(),
                    output_artifacts: Vec::new(),
                },
                timestamp,
                timestamp,
            )
            .await
            .expect("save trace");
        drop(store);

        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ModelClient::new(),
            Arc::new(
                ConfigStore::open_with_config_path(&db_path, &config_path)
                    .await
                    .expect("reopen store"),
            ),
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
        )
        .await
        .expect("runtime");

        let trace = runtime
            .tool_trace(agent_id, Some(session_id), "call_persisted".to_string())
            .await
            .expect("trace");
        assert_eq!(trace.turn_id, Some(turn_id));
        assert_eq!(trace.arguments["command"], "printf persisted");
        assert_eq!(trace.duration_ms, Some(99));
        assert_eq!(trace.output_preview, "persisted");
        assert_eq!(trace.started_at, Some(timestamp));
        assert_eq!(trace.completed_at, Some(timestamp));
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
            project_id: None,
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
            ModelClient::new(),
            Arc::new(
                ConfigStore::open_with_config_path(&db_path, &config_path)
                    .await
                    .expect("reopen store"),
            ),
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
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
                providers: vec![compact_no_continuation_test_provider(base_url)],
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
            .save_session_context_tokens(agent_id, session_id, 90)
            .await
            .expect("save tokens");
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ModelClient::new(),
            Arc::clone(&store),
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
        )
        .await
        .expect("runtime");
        let agent = runtime.agent(agent_id).await.expect("agent");

        let compacted = runtime
            .maybe_auto_compact(
                &agent,
                agent_id,
                session_id,
                Uuid::new_v4(),
                &CancellationToken::new(),
            )
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
            Some(90)
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
                "usage": { "input_tokens": 88, "output_tokens": 2, "total_tokens": 90 }
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
                providers: vec![compact_no_continuation_test_provider(base_url)],
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
            ModelClient::new(),
            Arc::clone(&store),
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
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
                CancellationToken::new(),
            )
            .await
            .expect("turn");

        let requests = requests.lock().await.clone();
        assert_eq!(requests.len(), 3);
        let expected_tool_count = build_tool_definitions_with_filter(&[], |_| true).len();
        assert_eq!(
            requests[0]["tools"].as_array().expect("first tools").len(),
            expected_tool_count
        );
        assert!(
            requests[1].get("tools").is_none(),
            "compact request should not send tools"
        );
        assert_eq!(
            requests[2]["tools"].as_array().expect("second tools").len(),
            expected_tool_count
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
                    && matches!(&content[0], ModelContentItem::InputText { text } if turn::history::is_compact_summary(text, COMPACT_SUMMARY_PREFIX) && text.contains("summary after tool output"))
        )));
        assert!(
            !session
                .history
                .iter()
                .any(|item| matches!(item, ModelInputItem::FunctionCallOutput { .. }))
        );
        assert_eq!(
            session
                .history
                .last()
                .and_then(turn::history::user_message_text),
            None
        );
        assert!(matches!(
            session.history.last(),
            Some(ModelInputItem::Message { role, content })
                if role == "assistant"
                    && matches!(&content[0], ModelContentItem::OutputText { text } if text == "final answer")
        ));
        assert!(runtime.events.snapshot().await.iter().any(|event| matches!(
            event.kind,
            ServiceEventKind::ContextCompacted {
                tokens_before: 90,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn turn_loop_has_no_tool_iteration_budget() {
        let mut responses = Vec::new();
        for i in 0..205 {
            responses.push(json!({
                "id": format!("tool_{i}"),
                "output": [{
                    "type": "function_call",
                    "call_id": format!("call_{i}"),
                    "name": "list_agents",
                    "arguments": "{}"
                }],
                "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
            }));
        }
        responses.push(json!({
            "id": "final",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "done after many tools" }]
            }],
            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
        }));
        let (base_url, requests) = start_mock_responses(responses).await;
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
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
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
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
                "keep going".to_string(),
                Vec::new(),
                CancellationToken::new(),
            )
            .await
            .expect("turn completes");

        assert_eq!(requests.lock().await.len(), 206);
        let (_, messages) = runtime
            .agent_recent_messages(agent_id, 4)
            .await
            .expect("messages");
        assert!(messages.iter().any(|message| {
            message.role == MessageRole::Assistant && message.content == "done after many tools"
        }));
    }

    #[tokio::test]
    async fn user_turn_includes_selected_skill_as_user_fragment() {
        let (base_url, requests) = start_mock_responses(vec![json!({
            "id": "skill",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "done" }]
            }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join(".agents/skills/demo");
        std::fs::create_dir_all(&skill_dir).expect("mkdir skill");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill.\n---\nUse the demo flow.",
        )
        .expect("write skill");
        let store = Arc::new(
            ConfigStore::open_with_config_path(
                dir.path().join("runtime.sqlite3"),
                dir.path().join("config.toml"),
            )
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
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let agent = runtime.agent(agent_id).await.expect("agent");
        *agent.container.write().await = Some(ContainerHandle {
            id: "container-1".to_string(),
            name: "container-1".to_string(),
            image: "unused".to_string(),
        });

        let turn_id = Uuid::new_v4();
        runtime
            .run_turn_inner(
                agent_id,
                session_id,
                turn_id,
                "please use $demo".to_string(),
                Vec::new(),
                CancellationToken::new(),
            )
            .await
            .expect("turn");

        let requests = requests.lock().await.clone();
        let input = requests[0]["input"].as_array().expect("input");
        assert!(input.iter().any(|item| {
            item["role"] == "user"
                && item["content"][0]["text"].as_str().is_some_and(|text| {
                    text.contains("<skill>")
                        && text.contains("<name>demo</name>")
                        && text.contains("Use the demo flow.")
                })
        }));
        let events = runtime.events.snapshot().await;
        let activated = events
            .iter()
            .find_map(|event| match &event.kind {
                ServiceEventKind::SkillsActivated {
                    agent_id: event_agent_id,
                    session_id: event_session_id,
                    turn_id: event_turn_id,
                    skills,
                } if *event_agent_id == agent_id
                    && *event_session_id == Some(session_id)
                    && *event_turn_id == turn_id =>
                {
                    Some(skills)
                }
                _ => None,
            })
            .expect("skills activated event");
        assert_eq!(activated.len(), 1);
        assert_eq!(activated[0].name, "demo");
        assert_eq!(activated[0].scope, mai_protocol::SkillScope::Repo);
    }

    #[tokio::test]
    async fn user_turn_semantic_match_is_available_but_not_runtime_injected() {
        let (base_url, requests) = start_mock_responses(vec![json!({
            "id": "skill",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "done" }]
            }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join(".agents/skills/frontend-app-builder");
        fs::create_dir_all(&skill_dir).expect("mkdir skill");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: frontend-app-builder\ndescription: Build frontend apps.\n---\nUse the frontend app builder flow.",
        )
        .expect("write skill");
        let store = test_store(&dir).await;
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
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
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
                "please use the frontend app builder".to_string(),
                Vec::new(),
                CancellationToken::new(),
            )
            .await
            .expect("turn");

        let requests = requests.lock().await.clone();
        let request_text = serde_json::to_string(&requests[0]).expect("request json");
        assert!(!request_text.contains("<name>frontend-app-builder</name>"));
        let instructions = requests[0]["instructions"].as_str().unwrap_or_default();
        assert!(instructions.contains("$frontend-app-builder"));
        assert!(instructions.contains("Build frontend apps."));
        assert!(instructions.contains("task clearly matches a skill's description"));
        assert!(
            !runtime
                .events
                .snapshot()
                .await
                .iter()
                .any(|event| matches!(event.kind, ServiceEventKind::SkillsActivated { .. }))
        );
    }

    #[tokio::test]
    async fn disabled_skill_is_not_injected() {
        let (base_url, requests) = start_mock_responses(vec![json!({
            "id": "skill",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "done" }]
            }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join(".agents/skills/demo");
        std::fs::create_dir_all(&skill_dir).expect("mkdir skill");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill.\n---\nUse the demo flow.",
        )
        .expect("write skill");
        let store = Arc::new(
            ConfigStore::open_with_config_path(
                dir.path().join("runtime.sqlite3"),
                dir.path().join("config.toml"),
            )
            .await
            .expect("open store"),
        );
        store
            .save_skills_config(&SkillsConfigRequest {
                config: vec![mai_protocol::SkillConfigEntry {
                    name: Some("demo".to_string()),
                    path: None,
                    enabled: false,
                }],
            })
            .await
            .expect("save skills config");
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
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
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
                "please use $demo".to_string(),
                Vec::new(),
                CancellationToken::new(),
            )
            .await
            .expect("turn");

        let request_text = serde_json::to_string(&requests.lock().await[0]).expect("request json");
        assert!(!request_text.contains("<skill>"));
        assert!(!request_text.contains("Use the demo flow."));
        assert!(
            !runtime
                .events
                .snapshot()
                .await
                .iter()
                .any(|event| matches!(event.kind, ServiceEventKind::SkillsActivated { .. }))
        );
    }

    #[tokio::test]
    async fn cancel_agent_turn_stops_model_request_and_marks_cancelled() {
        let (base_url, _requests) = start_mock_responses(vec![json!({
            "__delay_ms": 5_000,
            "id": "slow",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "too late" }]
            }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
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
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let turn_id = runtime
            .send_message(
                agent_id,
                Some(session_id),
                "slow please".to_string(),
                Vec::new(),
            )
            .await
            .expect("send");

        wait_until(
            || {
                let runtime = Arc::clone(&runtime);
                async move {
                    runtime
                        .agent(agent_id)
                        .await
                        .expect("agent")
                        .summary
                        .read()
                        .await
                        .current_turn
                        == Some(turn_id)
                }
            },
            Duration::from_secs(2),
        )
        .await;
        runtime
            .cancel_agent_turn(agent_id, turn_id)
            .await
            .expect("cancel");

        let summary = runtime
            .agent(agent_id)
            .await
            .expect("agent")
            .summary
            .read()
            .await
            .clone();
        assert_eq!(summary.status, AgentStatus::Cancelled);
        assert_eq!(summary.current_turn, None);
        assert!(runtime.events.snapshot().await.iter().any(|event| matches!(
            event.kind,
            ServiceEventKind::TurnCompleted {
                agent_id: event_agent_id,
                turn_id: event_turn_id,
                status: TurnStatus::Cancelled,
                ..
            } if event_agent_id == agent_id && event_turn_id == turn_id
        )));
    }

    #[tokio::test]
    async fn send_input_interrupt_starts_replacement_without_losing_message() {
        let (base_url, _requests) = start_mock_responses(vec![json!({
            "id": "replacement",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "replacement done" }]
            }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
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
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let old_turn_id = Uuid::new_v4();
        let agent = runtime.agent(agent_id).await.expect("agent");
        {
            let mut summary = agent.summary.write().await;
            summary.status = AgentStatus::RunningTurn;
            summary.current_turn = Some(old_turn_id);
        }
        *agent.container.write().await = Some(ContainerHandle {
            id: "container-1".to_string(),
            name: "container-1".to_string(),
            image: "unused".to_string(),
        });
        *agent.active_turn.lock().expect("active turn lock") = Some(TurnControl {
            turn_id: old_turn_id,
            session_id,
            cancellation_token: CancellationToken::new(),
            abort_handle: None,
        });

        let output = runtime
            .send_input_to_agent(
                agent_id,
                Some(session_id),
                "replacement".to_string(),
                Vec::new(),
                true,
            )
            .await
            .expect("interrupt");
        assert_eq!(output["queued"].as_bool(), Some(false));
        wait_until(
            || {
                let runtime = Arc::clone(&runtime);
                async move {
                    runtime
                        .agent(agent_id)
                        .await
                        .expect("agent")
                        .summary
                        .read()
                        .await
                        .current_turn
                        .is_none()
                }
            },
            Duration::from_secs(2),
        )
        .await;

        let detail = runtime
            .get_agent(agent_id, Some(session_id))
            .await
            .expect("detail");
        let message_dump = detail
            .messages
            .iter()
            .map(|message| format!("{:?}: {}", message.role, message.content))
            .collect::<Vec<_>>()
            .join(" | ");
        let event_dump = runtime
            .events
            .snapshot()
            .await
            .iter()
            .map(|event| format!("{:?}", event.kind))
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            detail.messages.iter().any(|message| {
                message.role == MessageRole::User && message.content == "replacement"
            }),
            "messages: {message_dump}; status: {:?}; events: {event_dump}",
            detail.summary.status
        );
        assert!(
            detail.messages.iter().any(|message| {
                message.role == MessageRole::Assistant && message.content == "replacement done"
            }),
            "messages: {message_dump}; status: {:?}; error: {:?}; events: {event_dump}",
            detail.summary.status,
            detail.summary.last_error
        );
    }

    #[tokio::test]
    async fn stale_turn_completion_does_not_overwrite_current_turn() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let agent_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        store
            .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
            .await
            .expect("save agent");
        save_test_session(&store, agent_id, session_id).await;
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let agent = runtime.agent(agent_id).await.expect("agent");
        let stale_turn_id = Uuid::new_v4();
        let current_turn_id = Uuid::new_v4();
        {
            let mut summary = agent.summary.write().await;
            summary.status = AgentStatus::RunningTurn;
            summary.current_turn = Some(current_turn_id);
        }
        *agent.active_turn.lock().expect("active turn lock") = Some(TurnControl {
            turn_id: current_turn_id,
            session_id,
            cancellation_token: CancellationToken::new(),
            abort_handle: None,
        });

        let completed = turn::completion::complete_turn_if_current(
            runtime.deps.store.as_ref(),
            &runtime.events,
            &agent,
            agent_id,
            TurnResult {
                turn_id: stale_turn_id,
                status: TurnStatus::Cancelled,
                agent_status: AgentStatus::Cancelled,
                final_text: None,
                error: None,
            },
        )
        .await
        .expect("complete stale");

        assert!(!completed);
        let summary = agent.summary.read().await.clone();
        assert_eq!(summary.status, AgentStatus::RunningTurn);
        assert_eq!(summary.current_turn, Some(current_turn_id));
        assert!(runtime.events.snapshot().await.iter().all(|event| {
            !matches!(
                event.kind,
                ServiceEventKind::TurnCompleted {
                    turn_id,
                    status: TurnStatus::Cancelled,
                    ..
                } if turn_id == stale_turn_id
            )
        }));
    }

    #[tokio::test]
    async fn save_artifact_uses_configured_artifact_roots() {
        let dir = tempdir().expect("tempdir");
        let artifact_index_root = dir.path().join("data/artifacts/index");
        let store = Arc::new(
            ConfigStore::open_with_config_and_artifact_index_path(
                dir.path().join("runtime.sqlite3"),
                dir.path().join("config.toml"),
                &artifact_index_root,
            )
            .await
            .expect("open store"),
        );
        let task_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, Some("created-container"));
        agent.task_id = Some(task_id);
        store.save_agent(&agent, None).await.expect("save agent");
        let plan = TaskPlan::default();
        let timestamp = now();
        let task = TaskSummary {
            id: task_id,
            title: "Artifact Task".to_string(),
            status: TaskStatus::Planning,
            plan_status: plan.status.clone(),
            plan_version: plan.version,
            planner_agent_id: agent_id,
            current_agent_id: Some(agent_id),
            agent_count: 1,
            review_rounds: 0,
            created_at: timestamp,
            updated_at: timestamp,
            last_error: None,
            final_report: None,
        };
        store.save_task(&task, &plan).await.expect("save task");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let agent_record = runtime.agent(agent_id).await.expect("agent");
        *agent_record.container.write().await = Some(ContainerHandle {
            id: "created-container".to_string(),
            name: "created-container".to_string(),
            image: "unused".to_string(),
        });

        let artifact = runtime
            .save_artifact(
                agent_id,
                "/workspace/report.txt".to_string(),
                Some("report.txt".to_string()),
            )
            .await
            .expect("save artifact");

        let file_path = dir
            .path()
            .join("data/artifacts/files")
            .join(task_id.to_string())
            .join(&artifact.id)
            .join("report.txt");
        assert_eq!(runtime.artifact_file_path(&artifact), file_path);
        assert_eq!(
            fs::read_to_string(&file_path).expect("artifact file"),
            "artifact\n"
        );
        assert!(
            artifact_index_root
                .join(format!("{}.json", artifact.id))
                .exists()
        );
        let artifacts = store.load_artifacts(&task_id).expect("load artifacts");
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].id, artifact.id);
        assert_eq!(artifacts[0].task_id, task_id);
        assert_eq!(artifacts[0].name, "report.txt");
        assert!(!dir.path().join("artifacts").exists());
    }

    #[tokio::test]
    async fn project_skill_cache_lists_project_scope_with_source_paths() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, Some("container-1"));
        agent.project_id = Some(project_id);
        save_agent_with_session(&store, &agent).await;
        let project = ready_test_project_summary(project_id, agent_id, "account-1");
        store.save_project(&project).await.expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let cache_dir = runtime.project_skill_cache_dir(project_id);
        assert_eq!(
            cache_dir,
            dir.path()
                .join("cache")
                .join(PROJECT_SKILLS_CACHE_DIR)
                .join(project_id.to_string())
        );
        assert!(!dir.path().join(PROJECT_SKILLS_CACHE_DIR).exists());
        let skill_dir = cache_dir.join("claude").join("demo");
        fs::create_dir_all(&skill_dir).expect("mkdir skill");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: project-demo\ndescription: Project demo skill.\n---\nUse project demo.",
        )
        .expect("write skill");

        let response = runtime
            .list_project_skills(project_id)
            .await
            .expect("project skills");

        assert!(response.errors.is_empty());
        assert_eq!(response.skills.len(), 1);
        assert_eq!(response.skills[0].scope, SkillScope::Project);
        assert_eq!(
            response.skills[0].source_path.as_deref(),
            Some(Path::new("/workspace/repo/.claude/skills/demo/SKILL.md"))
        );
        assert_eq!(
            response.roots,
            vec![PathBuf::from("/workspace/repo/.claude/skills")]
        );
    }

    #[tokio::test]
    async fn detects_project_skills_from_sidecar_candidate_dirs() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, Some("container-1"));
        agent.project_id = Some(project_id);
        save_agent_with_session(&store, &agent).await;
        let project = ready_test_project_summary(project_id, agent_id, "account-1");
        store.save_project(&project).await.expect("save project");
        let workspace = fake_sidecar_workspace_path(&dir);
        let claude_skill = workspace.join(".claude/skills/claude-demo");
        let agents_skill = workspace.join(".agents/skills/agents-demo");
        let root_skill = workspace.join("skills/root-demo");
        for (path, name) in [
            (&claude_skill, "claude-demo"),
            (&agents_skill, "agents-demo"),
            (&root_skill, "root-demo"),
        ] {
            fs::create_dir_all(path).expect("mkdir skill");
            fs::write(
                path.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name}\n---\nBody."),
            )
            .expect("write skill");
        }
        fs::create_dir_all(workspace.join("template/ignored")).expect("mkdir ignored");
        fs::write(
            workspace.join("template/ignored/SKILL.md"),
            "---\nname: ignored\ndescription: ignored\n---\nIgnored.",
        )
        .expect("write ignored");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let project = runtime.project(project_id).await.expect("project");
        *project.sidecar.write().await = Some(ContainerHandle {
            id: "created-container".to_string(),
            name: "sidecar".to_string(),
            image: "unused".to_string(),
        });

        let response = runtime
            .detect_project_skills(project_id)
            .await
            .expect("detect skills");

        let names = response
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["agents-demo", "claude-demo", "root-demo"]);
        assert!(
            response
                .skills
                .iter()
                .all(|skill| skill.scope == SkillScope::Project)
        );
        assert_eq!(response.roots.len(), 3);
        assert!(
            runtime
                .project_skill_cache_dir(project_id)
                .join("claude/claude-demo/SKILL.md")
                .exists()
        );
        assert!(!names.contains(&"ignored"));
    }

    #[tokio::test]
    async fn project_skill_refresh_serializes_cache_replacement() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, Some("created-container"));
        agent.project_id = Some(project_id);
        save_agent_with_session(&store, &agent).await;
        let project = ready_test_project_summary(project_id, agent_id, "account-1");
        store.save_project(&project).await.expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let project = runtime.project(project_id).await.expect("project");
        *project.sidecar.write().await = Some(ContainerHandle {
            id: "created-container".to_string(),
            name: "sidecar".to_string(),
            image: "unused".to_string(),
        });
        write_project_skill(
            &runtime,
            project_id,
            "serialized-refresh",
            "Old serialized skill.",
            "Old body.",
        );
        write_workspace_project_skill(
            &dir,
            ".claude/skills",
            "serialized-refresh",
            "New serialized skill.",
            "New body.",
        );
        let reader_runtime = Arc::clone(&runtime);
        let refresher_runtime = Arc::clone(&runtime);

        let (read_result, refresh_result) = tokio::join!(
            async move { reader_runtime.project_skills_from_cache(project_id).await },
            async move { refresher_runtime.detect_project_skills(project_id).await },
        );

        read_result.expect("read skills");
        refresh_result.expect("refresh skills");
        let response = runtime
            .project_skills_from_cache(project_id)
            .await
            .expect("skills after refresh");
        let skill = response
            .skills
            .iter()
            .find(|skill| skill.name == "serialized-refresh")
            .expect("serialized skill");
        assert_eq!(skill.description, "New serialized skill.");
        assert!(
            fs::read_to_string(&skill.path)
                .expect("skill body")
                .contains("New body.")
        );
    }

    #[tokio::test]
    async fn project_turn_injects_selected_project_skill_path() {
        let (base_url, requests) = start_mock_responses(vec![json!({
            "id": "project-skill",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "done" }]
            }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![compact_test_provider(base_url)],
                default_provider_id: Some("mock".to_string()),
            })
            .await
            .expect("save providers");
        store
            .upsert_git_account(GitAccountRequest {
                id: Some("account-1".to_string()),
                label: "GitHub".to_string(),
                token: Some("secret-token".to_string()),
                is_default: true,
                ..Default::default()
            })
            .await
            .expect("save account");
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, Some("container-1"));
        agent.project_id = Some(project_id);
        store.save_agent(&agent, None).await.expect("save agent");
        save_test_session(&store, agent_id, session_id).await;
        let project = ready_test_project_summary(project_id, agent_id, "account-1");
        store.save_project(&project).await.expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let agent_record = runtime.agent(agent_id).await.expect("agent");
        *agent_record.container.write().await = Some(ContainerHandle {
            id: "container-1".to_string(),
            name: "container-1".to_string(),
            image: "unused".to_string(),
        });
        let project = runtime.project(project_id).await.expect("project");
        *project.sidecar.write().await = Some(ContainerHandle {
            id: "created-container".to_string(),
            name: "sidecar".to_string(),
            image: "unused".to_string(),
        });
        let skill_dir = runtime
            .project_skill_cache_dir(project_id)
            .join("claude")
            .join("demo");
        fs::create_dir_all(&skill_dir).expect("mkdir skill");
        let skill_path = skill_dir.join("SKILL.md");
        fs::write(
            &skill_path,
            "---\nname: demo\ndescription: Project demo skill.\n---\nUse the project workflow.",
        )
        .expect("write skill");
        write_workspace_project_skill(
            &dir,
            ".claude/skills",
            "demo",
            "Project demo skill.",
            "Use the project workflow.",
        );

        let turn_id = Uuid::new_v4();
        runtime
            .run_turn_inner(
                agent_id,
                session_id,
                turn_id,
                "please help".to_string(),
                vec![skill_path.display().to_string()],
                CancellationToken::new(),
            )
            .await
            .expect("turn");

        let requests = requests.lock().await.clone();
        let input = requests[0]["input"].as_array().expect("input");
        assert!(input.iter().any(|item| {
            item["role"] == "user"
                && item["content"][0]["text"].as_str().is_some_and(|text| {
                    text.contains("<skill>") && text.contains("Use the project workflow.")
                })
        }));
        let instructions = requests[0]["instructions"].as_str().unwrap_or_default();
        assert!(instructions.contains("Project demo skill."));
        assert!(instructions.contains("/workspace/repo/.claude/skills/demo/SKILL.md"));
        let events = runtime.events.snapshot().await;
        let activated = events
            .iter()
            .find_map(|event| match &event.kind {
                ServiceEventKind::SkillsActivated {
                    agent_id: event_agent_id,
                    turn_id: event_turn_id,
                    skills,
                    ..
                } if *event_agent_id == agent_id && *event_turn_id == turn_id => Some(skills),
                _ => None,
            })
            .expect("skills activated");
        assert_eq!(activated.len(), 1);
        assert_eq!(activated[0].scope, SkillScope::Project);
    }

    #[tokio::test]
    async fn project_turn_refreshes_stale_project_skill_cache_before_injection() {
        let (base_url, requests) = start_mock_responses(vec![json!({
            "id": "project-skill-refresh",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "done" }]
            }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![compact_test_provider(base_url)],
                default_provider_id: Some("mock".to_string()),
            })
            .await
            .expect("save providers");
        store
            .upsert_git_account(GitAccountRequest {
                id: Some("account-1".to_string()),
                label: "GitHub".to_string(),
                token: Some("secret-token".to_string()),
                is_default: true,
                ..Default::default()
            })
            .await
            .expect("save account");
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, Some("created-container"));
        agent.project_id = Some(project_id);
        store.save_agent(&agent, None).await.expect("save agent");
        save_test_session(&store, agent_id, session_id).await;
        let project = ready_test_project_summary(project_id, agent_id, "account-1");
        store.save_project(&project).await.expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let agent_record = runtime.agent(agent_id).await.expect("agent");
        *agent_record.container.write().await = Some(ContainerHandle {
            id: "created-container".to_string(),
            name: "created-container".to_string(),
            image: "unused".to_string(),
        });
        let project = runtime.project(project_id).await.expect("project");
        *project.sidecar.write().await = Some(ContainerHandle {
            id: "created-container".to_string(),
            name: "sidecar".to_string(),
            image: "unused".to_string(),
        });
        write_project_skill(
            &runtime,
            project_id,
            "dynamic-demo",
            "Old project skill.",
            "Old cached body.",
        );
        write_workspace_project_skill(
            &dir,
            ".claude/skills",
            "dynamic-demo",
            "New project skill.",
            "New workspace body.",
        );

        runtime
            .run_turn_inner(
                agent_id,
                session_id,
                Uuid::new_v4(),
                "please use dynamic-demo".to_string(),
                vec!["dynamic-demo".to_string()],
                CancellationToken::new(),
            )
            .await
            .expect("turn");

        let requests = requests.lock().await.clone();
        let input = requests[0]["input"].as_array().expect("input");
        assert!(input.iter().any(|item| {
            item["role"] == "user"
                && item["content"][0]["text"].as_str().is_some_and(|text| {
                    text.contains("<name>dynamic-demo</name>")
                        && text.contains("New workspace body.")
                        && !text.contains("Old cached body.")
                })
        }));
        let instructions = requests[0]["instructions"].as_str().unwrap_or_default();
        assert!(instructions.contains("New project skill."));
    }

    #[tokio::test]
    async fn project_skill_plain_name_is_ambiguous_until_path_selected() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, Some("container-1"));
        agent.project_id = Some(project_id);
        save_agent_with_session(&store, &agent).await;
        let project = ready_test_project_summary(project_id, agent_id, "account-1");
        store.save_project(&project).await.expect("save project");
        let global_skill_dir = dir.path().join(".agents/skills/demo");
        fs::create_dir_all(&global_skill_dir).expect("mkdir global");
        fs::write(
            global_skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: Global demo.\n---\nGlobal.",
        )
        .expect("write global");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let project_skill_dir = runtime
            .project_skill_cache_dir(project_id)
            .join("claude")
            .join("demo");
        fs::create_dir_all(&project_skill_dir).expect("mkdir project");
        let project_skill_path = project_skill_dir.join("SKILL.md");
        fs::write(
            &project_skill_path,
            "---\nname: demo\ndescription: Project demo.\n---\nProject.",
        )
        .expect("write project");
        let skills_manager = runtime.skills_manager_with_project_roots(project_id);

        let ambiguous = skills_manager
            .build_injections(&["demo".to_string()], &SkillsConfigRequest::default())
            .expect("ambiguous injection");
        assert!(ambiguous.items.is_empty());

        let selected = skills_manager
            .build_injections(
                &[project_skill_path.display().to_string()],
                &SkillsConfigRequest::default(),
            )
            .expect("path injection");
        assert_eq!(selected.items.len(), 1);
        assert_eq!(selected.items[0].metadata.scope, SkillScope::Project);
    }

    #[tokio::test]
    async fn project_skill_detection_requires_ready_workspace() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, Some("container-1"));
        agent.project_id = Some(project_id);
        save_agent_with_session(&store, &agent).await;
        let project = test_project_summary(project_id, agent_id, "account-1");
        store.save_project(&project).await.expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        let err = runtime
            .detect_project_skills(project_id)
            .await
            .expect_err("not ready");

        assert!(err.to_string().contains("workspace is not ready"));
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
            ModelClient::new(),
            Arc::clone(&store),
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
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
                CancellationToken::new(),
            )
            .await;

        assert!(result.is_err());
        let events = runtime.events.snapshot().await;
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            ServiceEventKind::ToolCompleted {
                call_id,
                tool_name,
                success: true,
                ..
            } if call_id == "call_1" && tool_name == "list_agents"
        )));
        drop(events);
        let snapshot = store.load_runtime_snapshot(20).await.expect("snapshot");
        assert!(snapshot.agents[0].sessions[0].history.iter().any(|item| {
            matches!(
                item,
                ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_1"
            )
        }));
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
            ModelClient::new(),
            Arc::clone(&store),
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
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
            Some(90)
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
                    project_id: None,
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
            ModelClient::new(),
            store,
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
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
            ModelClient::new(),
            Arc::clone(&store),
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
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
    async fn role_model_resolution_falls_back_when_saved_provider_is_removed() {
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
        store
            .save_agent_config(&AgentConfigRequest {
                reviewer: Some(AgentModelPreference {
                    provider_id: "mimo-token-plan".to_string(),
                    model: "mimo-v1".to_string(),
                    reasoning_effort: None,
                }),
                ..Default::default()
            })
            .await
            .expect("save stale config");
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ModelClient::new(),
            Arc::clone(&store),
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
        )
        .await
        .expect("runtime");

        let resolved = runtime
            .resolve_role_agent_model(AgentRole::Reviewer)
            .await
            .expect("fallback reviewer model");

        assert_eq!(resolved.preference.provider_id, "openai");
        assert_eq!(resolved.preference.model, "gpt-5.5");
    }

    #[tokio::test]
    async fn project_detail_selects_live_reviewer_without_replacing_maintainer() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        maintainer.role = Some(AgentRole::Planner);
        let mut reviewer =
            test_agent_summary_with_parent(reviewer_id, None, Some("reviewer-container"));
        reviewer.project_id = Some(project_id);
        reviewer.role = Some(AgentRole::Reviewer);
        save_agent_with_session(&store, &maintainer).await;
        save_agent_with_session(&store, &reviewer).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        let detail = runtime
            .get_project(project_id, Some(reviewer_id), None)
            .await
            .expect("project detail");

        assert_eq!(detail.selected_agent_id, reviewer_id);
        assert_eq!(detail.selected_agent.summary.id, reviewer_id);
        assert_eq!(
            detail.selected_agent.summary.role,
            Some(AgentRole::Reviewer)
        );
        assert_eq!(detail.maintainer_agent.summary.id, maintainer_id);
        assert_eq!(
            detail.maintainer_agent.summary.role,
            Some(AgentRole::Planner)
        );
        assert!(detail.agents.iter().any(|agent| agent.id == maintainer_id));
        assert!(detail.agents.iter().any(|agent| agent.id == reviewer_id));
    }

    #[tokio::test]
    async fn project_detail_falls_back_to_maintainer_when_selected_reviewer_is_gone() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let reviewer_session_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        maintainer.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &maintainer).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        let detail = runtime
            .get_project(project_id, Some(reviewer_id), Some(reviewer_session_id))
            .await
            .expect("project detail");

        assert_eq!(detail.selected_agent_id, maintainer_id);
        assert_eq!(detail.selected_agent.summary.id, maintainer_id);
        assert_eq!(detail.maintainer_agent.summary.id, maintainer_id);
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
                    project_id: None,
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
        assert_eq!(child.provider_id, "openai");
        assert_eq!(child.model, "gpt-5.4");
        assert_eq!(child.reasoning_effort, Some("high".to_string()));
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
                    project_id: None,
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
    async fn spawn_agent_inherits_parent_and_accepts_codex_overrides() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider(), alt_test_provider()],
                default_provider_id: Some("openai".to_string()),
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
                    project_id: None,
                    role: None,
                    name: "parent".to_string(),
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
                },
                None,
            )
            .await
            .expect("save parent");
        save_test_session(&store, parent_id, Uuid::new_v4()).await;
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
                    "agent_type": "worker",
                    "model": "gpt-5.4",
                    "reasoning_effort": "high",
                    "message": "start"
                }),
            )
            .await
            .expect("spawn");
        assert!(result.success);
        let child = runtime
            .list_agents()
            .await
            .into_iter()
            .find(|agent| agent.parent_id == Some(parent_id))
            .expect("child");
        assert_eq!(child.provider_id, "openai");
        assert_eq!(child.model, "gpt-5.4");
        assert_eq!(child.reasoning_effort, Some("high".to_string()));
        assert_eq!(child.role, Some(AgentRole::Executor));
    }

    #[tokio::test]
    async fn spawn_agent_skill_item_injects_child_initial_turn() {
        let (base_url, requests) = start_mock_responses(vec![json!({
            "id": "child-skill",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "child done" }]
            }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join(".agents/skills/demo");
        fs::create_dir_all(&skill_dir).expect("mkdir skill");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill.\n---\nUse child demo.",
        )
        .expect("write skill");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![compact_test_provider(base_url)],
                default_provider_id: Some("mock".to_string()),
            })
            .await
            .expect("save providers");
        let parent_id = Uuid::new_v4();
        store
            .save_agent(
                &test_agent_summary(parent_id, Some("parent-container")),
                None,
            )
            .await
            .expect("save parent");
        save_test_session(&store, parent_id, Uuid::new_v4()).await;
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
                    "items": [
                        { "type": "text", "text": "child task" },
                        { "type": "skill", "name": "demo" }
                    ]
                }),
            )
            .await
            .expect("spawn");
        assert!(result.success);

        wait_until(
            || {
                let requests = Arc::clone(&requests);
                async move { !requests.lock().await.is_empty() }
            },
            Duration::from_secs(2),
        )
        .await;
        let requests = requests.lock().await.clone();
        let input = requests[0]["input"].as_array().expect("input");
        assert!(input.iter().any(|item| {
            item["role"] == "user"
                && item["content"][0]["text"].as_str().is_some_and(|text| {
                    text.contains("<name>demo</name>") && text.contains("Use child demo.")
                })
        }));
    }

    #[tokio::test]
    async fn project_subagent_refreshes_and_reads_new_project_skill_resource() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        let mut child =
            test_agent_summary_with_parent(child_id, Some(maintainer_id), Some("child-container"));
        child.project_id = Some(project_id);
        child.role = Some(AgentRole::Explorer);
        save_agent_with_session(&store, &maintainer).await;
        save_agent_with_session(&store, &child).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let project = runtime.project(project_id).await.expect("project");
        *project.sidecar.write().await = Some(ContainerHandle {
            id: "created-container".to_string(),
            name: "sidecar".to_string(),
            image: "unused".to_string(),
        });
        write_workspace_project_skill(
            &dir,
            ".claude/skills",
            "fresh-child-skill",
            "Fresh child skill.",
            "Fresh child body.",
        );

        runtime
            .refresh_project_skills_for_agent(&runtime.agent(child_id).await.expect("child"))
            .await
            .expect("refresh");
        let result = runtime
            .execute_tool_for_test(
                child_id,
                "read_mcp_resource",
                json!({
                    "server": "project-skill",
                    "uri": "skill:///fresh-child-skill"
                }),
            )
            .await
            .expect("read skill");

        let output: Value = serde_json::from_str(&result.output).expect("json output");
        assert!(
            output["contents"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("Fresh child body.")
        );
    }

    #[tokio::test]
    async fn project_subagent_turn_syncs_project_skill_to_container() {
        let (base_url, _requests) = start_mock_responses(vec![json!({
            "id": "project-child-skill",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "child done" }]
            }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![compact_test_provider(base_url)],
                default_provider_id: Some("mock".to_string()),
            })
            .await
            .expect("save providers");
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        let mut child =
            test_agent_summary_with_parent(child_id, Some(maintainer_id), Some("child-container"));
        child.project_id = Some(project_id);
        child.role = Some(AgentRole::Explorer);
        save_agent_with_session(&store, &maintainer).await;
        let session_id = Uuid::new_v4();
        store.save_agent(&child, None).await.expect("save child");
        save_test_session(&store, child_id, session_id).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        runtime.state.project_mcp_managers.write().await.insert(
            project_id,
            Arc::new(McpAgentManager::from_tools_for_test(Vec::new())),
        );
        let project = runtime.project(project_id).await.expect("project");
        *project.sidecar.write().await = Some(ContainerHandle {
            id: "created-container".to_string(),
            name: "sidecar".to_string(),
            image: "unused".to_string(),
        });
        let child_record = runtime.agent(child_id).await.expect("child");
        *child_record.container.write().await = Some(ContainerHandle {
            id: "child-container".to_string(),
            name: "child".to_string(),
            image: "unused".to_string(),
        });
        write_workspace_project_skill(
            &dir,
            ".claude/skills",
            "fresh-child-skill",
            "Fresh child skill.",
            "Fresh child body.",
        );

        runtime
            .run_turn_inner(
                child_id,
                session_id,
                Uuid::new_v4(),
                "Use $fresh-child-skill".to_string(),
                Vec::new(),
                CancellationToken::new(),
            )
            .await
            .expect("turn");

        let docker_log = fake_docker_log(&dir);
        assert!(
            docker_log.contains("cp")
                && docker_log.contains("/workspace/.mai-team/skills/project/fresh-child-skill")
        );
    }

    #[tokio::test]
    async fn project_worker_cannot_spawn_agents_and_hidden_from_tools() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let worker_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        maintainer.role = Some(AgentRole::Planner);
        let mut worker = test_agent_summary_with_parent(
            worker_id,
            Some(maintainer_id),
            Some("worker-container"),
        );
        worker.project_id = Some(project_id);
        worker.role = Some(AgentRole::Executor);
        save_agent_with_session(&store, &maintainer).await;
        save_agent_with_session(&store, &worker).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let worker_record = runtime.agent(worker_id).await.expect("worker");

        let visible = turn::tools::visible_tool_names(&runtime.state, &worker_record, &[]).await;
        assert!(!visible.contains(mai_tools::TOOL_SPAWN_AGENT));
        assert!(!visible.contains(mai_tools::TOOL_CLOSE_AGENT));

        let result = runtime
            .execute_tool_for_test(
                worker_id,
                "spawn_agent",
                json!({
                    "message": "should fail"
                }),
            )
            .await;

        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(message)) if message.contains("spawn_agent"))
        );
    }

    #[tokio::test]
    async fn project_maintainer_can_spawn_agent() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![compact_test_provider("http://localhost".to_string())],
                default_provider_id: Some("mock".to_string()),
            })
            .await
            .expect("save providers");
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        maintainer.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &maintainer).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let maintainer_record = runtime.agent(maintainer_id).await.expect("maintainer");
        *maintainer_record.container.write().await = Some(ContainerHandle {
            id: "maintainer-container".to_string(),
            name: "maintainer-container".to_string(),
            image: "unused".to_string(),
        });

        let visible =
            turn::tools::visible_tool_names(&runtime.state, &maintainer_record, &[]).await;
        assert!(visible.contains(mai_tools::TOOL_SPAWN_AGENT));

        let result = runtime
            .execute_tool_for_test(
                maintainer_id,
                "spawn_agent",
                json!({
                    "agent_type": "worker"
                }),
            )
            .await
            .expect("spawn");

        assert!(result.success);
    }

    #[tokio::test]
    async fn project_agent_without_discovered_mcp_tools_has_no_static_fallback() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        save_agent_with_session(&store, &maintainer).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let maintainer_record = runtime.agent(maintainer_id).await.expect("maintainer");

        let tools = runtime.agent_mcp_tools(&maintainer_record).await;

        assert!(tools.is_empty());
        let visible =
            turn::tools::visible_tool_names(&runtime.state, &maintainer_record, &tools).await;
        assert!(!visible.contains("mcp__github__create_pull_request_review"));
        assert!(!visible.contains("mcp__github__pull_request_review_write"));
        assert!(!visible.contains("mcp__git__git_status"));
    }

    #[tokio::test]
    async fn project_agent_mcp_tools_match_project_manager_discovery() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        save_agent_with_session(&store, &maintainer).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let discovered = vec![
            test_mcp_tool("github", "pull_request_review_write"),
            test_mcp_tool("git", "git_diff_unstaged"),
        ];
        runtime.state.project_mcp_managers.write().await.insert(
            project_id,
            Arc::new(McpAgentManager::from_tools_for_test(discovered.clone())),
        );
        let maintainer_record = runtime.agent(maintainer_id).await.expect("maintainer");

        let tools = runtime.agent_mcp_tools(&maintainer_record).await;

        let names = tools
            .iter()
            .map(|tool| tool.model_name.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(
            names,
            HashSet::from([
                "mcp__github__pull_request_review_write",
                "mcp__git__git_diff_unstaged",
            ])
        );
        assert_eq!(tools.len(), discovered.len());
        let visible =
            turn::tools::visible_tool_names(&runtime.state, &maintainer_record, &tools).await;
        assert!(visible.contains("mcp__github__pull_request_review_write"));
        assert!(visible.contains("mcp__git__git_diff_unstaged"));
        assert!(!visible.contains("mcp__github__create_pull_request_review"));
        assert!(!visible.contains("mcp__git__git_status"));
    }

    #[tokio::test]
    async fn project_reviewer_reads_project_skill_resource_without_mcp_session() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        maintainer.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &maintainer).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        write_project_skill(
            &runtime,
            project_id,
            "review-open-prs",
            "Review open PRs.",
            "Use the review workflow.",
        );
        let reviewer = runtime
            .spawn_project_reviewer_agent(project_id)
            .await
            .expect("spawn reviewer");

        let result = runtime
            .execute_tool_for_test(
                reviewer.id,
                "read_mcp_resource",
                json!({
                    "server": "project-skill",
                    "uri": "skill:///review-open-prs"
                }),
            )
            .await
            .expect("read skill resource");

        assert!(result.success);
        let output: Value = serde_json::from_str(&result.output).expect("json output");
        let text = output["contents"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("name: review-open-prs"));
        assert!(text.contains("Use the review workflow."));
    }

    #[tokio::test]
    async fn skill_resource_can_read_bundled_relative_file() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let system_skill_dir = dir.path().join("system-skills").join("demo");
        fs::create_dir_all(system_skill_dir.join("scripts")).expect("mkdir skill");
        fs::write(
            system_skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill.\n---\nDemo body.",
        )
        .expect("write skill");
        fs::write(
            system_skill_dir.join("scripts/helper.py"),
            "print('hello from helper')\n",
        )
        .expect("write helper");
        let agent_id = Uuid::new_v4();
        save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container-1"))).await;
        let runtime = AgentRuntime::new(
            DockerClient::new_with_binary("unused", fake_docker_path(&dir)),
            ModelClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                system_skills_root: Some(dir.path().join("system-skills")),
                ..test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE)
            },
        )
        .await
        .expect("runtime");

        let result = runtime
            .execute_tool_for_test(
                agent_id,
                "read_mcp_resource",
                json!({
                    "server": "skill",
                    "uri": "skill:///demo/scripts/helper.py"
                }),
            )
            .await
            .expect("read helper");

        assert!(result.success);
        let output: Value = serde_json::from_str(&result.output).expect("json output");
        assert_eq!(output["contents"][0]["mimeType"], "text/x-python");
        assert!(
            output["contents"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("hello from helper")
        );
    }

    #[tokio::test]
    async fn project_subagent_inherits_project_skill_resources() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        save_agent_with_session(&store, &maintainer).await;
        let mut child =
            test_agent_summary_with_parent(child_id, Some(maintainer_id), Some("child-container"));
        child.project_id = Some(project_id);
        child.role = Some(AgentRole::Explorer);
        save_agent_with_session(&store, &child).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        write_project_skill(
            &runtime,
            project_id,
            "review-open-prs",
            "Review open PRs.",
            "Inherited by child agents.",
        );

        let result = runtime
            .execute_tool_for_test(
                child_id,
                "read_mcp_resource",
                json!({
                    "server": "project-skill",
                    "uri": "skill:///review-open-prs"
                }),
            )
            .await
            .expect("child reads skill");

        let output: Value = serde_json::from_str(&result.output).expect("json output");
        assert!(
            output["contents"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("Inherited by child agents.")
        );
    }

    #[tokio::test]
    async fn project_agent_lists_project_mcp_and_skill_resources() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        save_agent_with_session(&store, &maintainer).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        write_project_skill(
            &runtime,
            project_id,
            "review-open-prs",
            "Review open PRs.",
            "Use the review workflow.",
        );
        runtime.state.project_mcp_managers.write().await.insert(
            project_id,
            Arc::new(McpAgentManager::from_resources_for_test(vec![(
                "github",
                vec![json!({
                    "uri": "github://pulls",
                    "name": "pulls",
                    "mimeType": "application/json"
                })],
            )])),
        );

        let result = runtime
            .execute_tool_for_test(maintainer_id, "list_mcp_resources", json!({}))
            .await
            .expect("list resources");

        let output: Value = serde_json::from_str(&result.output).expect("json output");
        let uris = output["resources"]
            .as_array()
            .expect("resources")
            .iter()
            .filter_map(|item| item.get("uri").and_then(Value::as_str))
            .collect::<HashSet<_>>();
        assert!(uris.contains("skill:///review-open-prs"));
        assert!(uris.contains("github://pulls"));
    }

    #[tokio::test]
    async fn unknown_resource_provider_returns_clear_error() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let agent_id = Uuid::new_v4();
        save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container"))).await;
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        let err = runtime
            .execute_tool_for_test(
                agent_id,
                "read_mcp_resource",
                json!({
                    "server": "missing-provider",
                    "uri": "missing://resource"
                }),
            )
            .await
            .expect_err("missing provider");

        let message = err.to_string();
        assert!(message.contains("resource provider not found: missing-provider"));
        assert!(!message.contains("session not found"));
    }

    #[tokio::test]
    async fn task_agent_reads_agent_mcp_resources() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let agent_id = Uuid::new_v4();
        save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container"))).await;
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let agent = runtime.agent(agent_id).await.expect("agent");
        *agent.mcp.write().await =
            Some(Arc::new(McpAgentManager::from_resources_for_test(vec![(
                "agent-docs",
                vec![json!({
                    "uri": "agent://docs",
                    "name": "docs",
                    "mimeType": "text/plain"
                })],
            )])));

        let result = runtime
            .execute_tool_for_test(
                agent_id,
                "read_mcp_resource",
                json!({
                    "server": "agent-docs",
                    "uri": "agent://docs"
                }),
            )
            .await
            .expect("read agent resource");

        let output: Value = serde_json::from_str(&result.output).expect("json output");
        assert_eq!(output["contents"][0]["uri"], "agent://docs");
    }

    #[test]
    fn project_mcp_configs_use_official_defaults_without_git_token_env() {
        let configs = projects::mcp::project_mcp_configs("secret-token");
        let github = configs.get("github").expect("github");
        assert_eq!(
            github
                .env
                .get("GITHUB_PERSONAL_ACCESS_TOKEN")
                .map(String::as_str),
            Some("secret-token")
        );
        assert_eq!(
            github.env.get("GITHUB_TOOLSETS").map(String::as_str),
            Some("context,repos,issues,pull_requests")
        );
        let git = configs.get("git").expect("git");
        assert_eq!(git.command.as_deref(), Some("uvx"));
        assert_eq!(
            git.args,
            vec![
                "mcp-server-git".to_string(),
                "--repository".to_string(),
                PROJECT_WORKSPACE_PATH.to_string(),
            ]
        );
        assert!(!git.env.contains_key("GITHUB_TOKEN"));
    }

    #[tokio::test]
    async fn wait_agent_accepts_targets_and_send_input_queues_busy_target() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let parent_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let child_session_id = Uuid::new_v4();
        let timestamp = now();
        store
            .save_agent(&test_agent_summary_at(parent_id, None, timestamp), None)
            .await
            .expect("save parent");
        save_test_session(&store, parent_id, Uuid::new_v4()).await;
        let mut child = test_agent_summary_at(child_id, Some(parent_id), timestamp);
        child.status = AgentStatus::RunningTurn;
        child.current_turn = Some(Uuid::new_v4());
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
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let child_record = runtime.agent(child_id).await.expect("child");
        {
            let mut summary = child_record.summary.write().await;
            summary.status = AgentStatus::RunningTurn;
            summary.current_turn = Some(Uuid::new_v4());
        }

        let queued = runtime
            .execute_tool_for_test(
                parent_id,
                "send_input",
                json!({
                    "target": child_id.to_string(),
                    "items": [{ "type": "text", "text": "queued hello" }]
                }),
            )
            .await
            .expect("send input");
        assert!(queued.success);
        let value: Value = serde_json::from_str(&queued.output).expect("json");
        assert_eq!(value["queued"].as_bool(), Some(true));

        let waited = runtime
            .execute_tool_for_test(
                parent_id,
                "wait_agent",
                json!({
                    "targets": [child_id.to_string()],
                    "timeout_ms": 1
                }),
            )
            .await
            .expect("wait");
        let value: Value = serde_json::from_str(&waited.output).expect("json");
        assert!(value["completed"].as_array().expect("completed").is_empty());
        let pending = value["pending"].as_array().expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0]["agent_id"].as_str(),
            Some(child_id.to_string().as_str())
        );
        assert_eq!(pending[0]["status"].as_str(), Some("running_turn"));
        assert_eq!(
            pending[0]["diagnostics"]["current_turn"].as_str(),
            pending[0]["current_turn"].as_str()
        );
        assert!(pending[0]["diagnostics"]["idle_ms"].as_u64().is_some());
        assert_eq!(value["timed_out"].as_bool(), Some(true));
    }

    #[tokio::test]
    async fn send_input_queued_skill_item_is_preserved_for_next_turn() {
        let (base_url, requests) = start_mock_responses(vec![json!({
            "id": "queued-skill",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "queued done" }]
            }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join(".agents/skills/demo");
        fs::create_dir_all(&skill_dir).expect("mkdir skill");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill.\n---\nQueued demo body.",
        )
        .expect("write skill");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![compact_test_provider(base_url)],
                default_provider_id: Some("mock".to_string()),
            })
            .await
            .expect("save providers");
        let parent_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let child_session_id = Uuid::new_v4();
        let timestamp = now();
        store
            .save_agent(&test_agent_summary_at(parent_id, None, timestamp), None)
            .await
            .expect("save parent");
        save_test_session(&store, parent_id, Uuid::new_v4()).await;
        let mut child = test_agent_summary_at(child_id, Some(parent_id), timestamp);
        child.status = AgentStatus::RunningTurn;
        child.current_turn = Some(Uuid::new_v4());
        child.container_id = Some("child-container".to_string());
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
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let child_record = runtime.agent(child_id).await.expect("child");
        {
            let mut summary = child_record.summary.write().await;
            summary.status = AgentStatus::RunningTurn;
            summary.current_turn = Some(Uuid::new_v4());
            summary.container_id = Some("child-container".to_string());
        }
        *child_record.container.write().await = Some(ContainerHandle {
            id: "child-container".to_string(),
            name: "child-container".to_string(),
            image: "unused".to_string(),
        });

        let queued = runtime
            .execute_tool_for_test(
                parent_id,
                "send_input",
                json!({
                    "target": child_id.to_string(),
                    "items": [
                        { "type": "text", "text": "queued hello" },
                        { "type": "skill", "name": "demo" }
                    ]
                }),
            )
            .await
            .expect("send input");
        assert!(queued.success);
        {
            let mut summary = child_record.summary.write().await;
            summary.status = AgentStatus::Idle;
            summary.current_turn = None;
        }
        agents::start_next_queued_input(runtime.as_ref(), &runtime, child_id)
            .await
            .expect("start queued");

        wait_until(
            || {
                let requests = Arc::clone(&requests);
                async move { !requests.lock().await.is_empty() }
            },
            Duration::from_secs(2),
        )
        .await;
        let requests = requests.lock().await.clone();
        let input = requests[0]["input"].as_array().expect("input");
        assert!(input.iter().any(|item| {
            item["role"] == "user"
                && item["content"][0]["text"].as_str().is_some_and(|text| {
                    text.contains("<name>demo</name>") && text.contains("Queued demo body.")
                })
        }));
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
            ModelClient::new(),
            Arc::clone(&store),
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
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

    #[test]
    fn project_maintainer_prompt_includes_clone_url_and_workspace() {
        let prompt = project_maintainer_system_prompt(
            "owner",
            "repo",
            "https://github.com/owner/repo.git",
            "main",
        );

        assert!(prompt.contains("https://github.com/owner/repo.git"));
        assert!(prompt.contains("/workspace/repo"));
    }

    #[tokio::test]
    async fn project_clone_uses_configured_sidecar_image_and_execs_inside_sidecar() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        store
            .upsert_git_account(GitAccountRequest {
                id: Some("account-1".to_string()),
                label: "GitHub".to_string(),
                token: Some("secret-token".to_string()),
                is_default: true,
                ..Default::default()
            })
            .await
            .expect("save account");
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, None);
        agent.project_id = Some(project_id);
        agent.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &agent).await;
        let project = test_project_summary(project_id, agent_id, "account-1");
        store.save_project(&project).await.expect("save project");
        let runtime = test_runtime_with_sidecar_image_and_git(
            &dir,
            Arc::clone(&store),
            "ghcr.io/example/mai-team-sidecar:test",
        )
        .await;

        runtime
            .clone_project_repository(project_id, agent_id)
            .await
            .expect("clone");

        let docker_log = fake_docker_log(&dir);
        assert!(docker_log.contains(&format!(
            "create --name mai-team-project-sidecar-{project_id}"
        )));
        assert!(docker_log.contains("ghcr.io/example/mai-team-sidecar:test sleep infinity"));
        assert!(docker_log.contains("exec -w / created-container /bin/sh -lc"));
        assert!(docker_log.contains("rm -rf"));
        assert!(docker_log.contains("/workspace/repo"));
        assert!(docker_log.contains("git -c credential.helper= clone"));
        assert!(docker_log.contains("sidecar-git-clone"));
        assert!(docker_log.contains("chown -R"));
        assert!(docker_log.contains("safe.directory"));
        let git_log = fake_git_log(&dir);
        assert!(git_log.is_empty());
        assert!(docker_log.contains("token-present"));
        assert!(docker_log.contains("created-container"));
        assert!(!docker_log.contains("unused-agent sleep infinity"));
        assert!(!docker_log.contains("secret-token"));
        assert!(!git_log.contains("secret-token"));
    }

    #[tokio::test]
    async fn project_workspace_setup_moves_from_pending_to_ready() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        store
            .upsert_git_account(GitAccountRequest {
                id: Some("account-1".to_string()),
                label: "GitHub".to_string(),
                token: Some("secret-token".to_string()),
                is_default: true,
                ..Default::default()
            })
            .await
            .expect("save account");
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, None);
        agent.status = AgentStatus::Created;
        agent.project_id = Some(project_id);
        agent.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &agent).await;
        let project = test_project_summary(project_id, agent_id, "account-1");
        store.save_project(&project).await.expect("save project");
        let runtime = test_runtime_with_sidecar_image_and_git(
            &dir,
            Arc::clone(&store),
            "ghcr.io/example/mai-team-sidecar:test",
        )
        .await;
        let mut events = runtime.subscribe();

        runtime
            .start_project_workspace(project_id, agent_id)
            .await
            .expect("setup");

        let detail = runtime
            .get_project(project_id, None, None)
            .await
            .expect("detail");
        assert_eq!(detail.summary.status, ProjectStatus::Ready);
        assert_eq!(detail.summary.clone_status, ProjectCloneStatus::Ready);
        assert_eq!(detail.maintainer_agent.summary.status, AgentStatus::Idle);
        let docker_log = fake_docker_log(&dir);
        assert!(docker_log.contains("git -c credential.helper= clone"));
        assert!(docker_log.contains("https://github.com/owner/repo.git"));
        assert!(fake_git_log(&dir).is_empty());

        let mut saw_cloning = false;
        let mut saw_ready = false;
        while let Ok(event) = events.try_recv() {
            match event.kind {
                ServiceEventKind::ProjectUpdated { project }
                    if project.id == project_id
                        && project.clone_status == ProjectCloneStatus::Cloning =>
                {
                    saw_cloning = true;
                }
                ServiceEventKind::ProjectUpdated { project }
                    if project.id == project_id
                        && project.clone_status == ProjectCloneStatus::Ready =>
                {
                    saw_ready = true;
                }
                _ => {}
            }
        }
        assert!(saw_cloning);
        assert!(saw_ready);
    }

    #[tokio::test]
    async fn runtime_start_starts_auto_review_worker_immediately() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        store
            .upsert_git_account(GitAccountRequest {
                id: Some("account-1".to_string()),
                label: "GitHub".to_string(),
                token: Some("secret-token".to_string()),
                is_default: true,
                ..Default::default()
            })
            .await
            .expect("save account");
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, Some("maintainer-container"));
        agent.project_id = Some(project_id);
        agent.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &agent).await;
        let mut project = ready_test_project_summary(project_id, agent_id, "account-1");
        project.auto_review_enabled = true;
        project.review_status = ProjectReviewStatus::Waiting;
        project.next_review_at = Some(now() + TimeDelta::minutes(30));
        store.save_project(&project).await.expect("save project");

        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let project_record = runtime.project(project_id).await.expect("project");

        wait_until(
            || {
                let project_record = Arc::clone(&project_record);
                async move { project_record.review_worker.lock().await.is_some() }
            },
            Duration::from_secs(2),
        )
        .await;
        runtime.stop_project_review_loop(project_id).await;
    }

    #[tokio::test]
    async fn runtime_start_cleans_stale_project_reviewer_before_new_worker() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        store
            .upsert_git_account(GitAccountRequest {
                id: Some("account-1".to_string()),
                label: "GitHub".to_string(),
                token: Some("secret-token".to_string()),
                is_default: true,
                ..Default::default()
            })
            .await
            .expect("save account");
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        maintainer.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &maintainer).await;
        let mut reviewer = test_agent_summary_with_parent(
            reviewer_id,
            Some(maintainer_id),
            Some("reviewer-container"),
        );
        reviewer.project_id = Some(project_id);
        reviewer.role = Some(AgentRole::Reviewer);
        reviewer.status = AgentStatus::RunningTurn;
        reviewer.current_turn = Some(turn_id);
        save_agent_with_session(&store, &reviewer).await;
        let mut project = ready_test_project_summary(project_id, maintainer_id, "account-1");
        project.auto_review_enabled = true;
        project.review_status = ProjectReviewStatus::Running;
        project.current_reviewer_agent_id = Some(reviewer_id);
        store.save_project(&project).await.expect("save project");
        store
            .save_project_review_run(&ProjectReviewRunDetail {
                summary: ProjectReviewRunSummary {
                    id: run_id,
                    project_id,
                    reviewer_agent_id: Some(reviewer_id),
                    turn_id: Some(turn_id),
                    started_at: now(),
                    finished_at: None,
                    status: ProjectReviewRunStatus::Running,
                    outcome: None,
                    pr: None,
                    summary: Some("in progress".to_string()),
                    error: None,
                },
                messages: Vec::new(),
                events: Vec::new(),
            })
            .await
            .expect("save run");

        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let project_record = runtime.project(project_id).await.expect("project");

        wait_until(
            || {
                let project_record = Arc::clone(&project_record);
                async move { project_record.review_worker.lock().await.is_some() }
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(matches!(
            runtime.agent(reviewer_id).await,
            Err(RuntimeError::AgentNotFound(id)) if id == reviewer_id
        ));
        let run = runtime
            .get_project_review_run(project_id, run_id)
            .await
            .expect("run");
        assert_eq!(run.summary.status, ProjectReviewRunStatus::Cancelled);
        assert_eq!(
            run.summary.error.as_deref(),
            Some("review interrupted by server restart")
        );
        let project = runtime.project(project_id).await.expect("project");
        let summary = project.summary.read().await.clone();
        assert_eq!(summary.current_reviewer_agent_id, None);
        assert_eq!(summary.next_review_at, None);
        runtime.stop_project_review_loop(project_id).await;
    }

    #[tokio::test]
    async fn runtime_start_deletes_orphan_project_reviewer() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        store
            .upsert_git_account(GitAccountRequest {
                id: Some("account-1".to_string()),
                label: "GitHub".to_string(),
                token: Some("secret-token".to_string()),
                is_default: true,
                ..Default::default()
            })
            .await
            .expect("save account");
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let orphan_reviewer_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        maintainer.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &maintainer).await;
        let mut reviewer = test_agent_summary_with_parent(
            orphan_reviewer_id,
            Some(maintainer_id),
            Some("orphan-reviewer-container"),
        );
        reviewer.project_id = Some(project_id);
        reviewer.role = Some(AgentRole::Reviewer);
        reviewer.status = AgentStatus::RunningTurn;
        reviewer.current_turn = Some(Uuid::new_v4());
        save_agent_with_session(&store, &reviewer).await;
        let mut project = ready_test_project_summary(project_id, maintainer_id, "account-1");
        project.auto_review_enabled = true;
        project.review_status = ProjectReviewStatus::Idle;
        store.save_project(&project).await.expect("save project");

        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        assert!(matches!(
            runtime.agent(orphan_reviewer_id).await,
            Err(RuntimeError::AgentNotFound(id)) if id == orphan_reviewer_id
        ));
        let project_record = runtime.project(project_id).await.expect("project");
        assert!(project_record.review_worker.lock().await.is_some());
        runtime.stop_project_review_loop(project_id).await;
    }

    #[tokio::test]
    async fn runtime_start_reviewer_singleton_keeps_non_reviewer_project_agents() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let executor_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        maintainer.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &maintainer).await;
        let mut executor = test_agent_summary_with_parent(
            executor_id,
            Some(maintainer_id),
            Some("executor-container"),
        );
        executor.project_id = Some(project_id);
        executor.role = Some(AgentRole::Executor);
        executor.status = AgentStatus::RunningTurn;
        executor.current_turn = Some(Uuid::new_v4());
        save_agent_with_session(&store, &executor).await;
        let mut project = test_project_summary(project_id, maintainer_id, "account-1");
        project.auto_review_enabled = true;
        project.review_status = ProjectReviewStatus::Idle;
        store.save_project(&project).await.expect("save project");

        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        runtime.agent(maintainer_id).await.expect("maintainer");
        runtime.agent(executor_id).await.expect("executor");
        let project_record = runtime.project(project_id).await.expect("project");
        assert!(project_record.review_worker.lock().await.is_none());
    }

    #[tokio::test]
    async fn runtime_start_does_not_start_auto_review_for_not_ready_project() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, Some("maintainer-container"));
        agent.project_id = Some(project_id);
        agent.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &agent).await;
        let mut project = test_project_summary(project_id, agent_id, "account-1");
        project.auto_review_enabled = true;
        project.review_status = ProjectReviewStatus::Idle;
        store.save_project(&project).await.expect("save project");

        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let project_record = runtime.project(project_id).await.expect("project");

        assert!(project_record.review_worker.lock().await.is_none());
    }

    #[tokio::test]
    async fn project_reviewer_starts_from_image_with_review_workspace_without_snapshot() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        maintainer.role = Some(AgentRole::Planner);
        maintainer.docker_image = "ghcr.io/rcore-os/tgoskits-container:latest".to_string();
        save_agent_with_session(&store, &maintainer).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        let reviewer = runtime
            .spawn_project_reviewer_agent(project_id)
            .await
            .expect("spawn reviewer");

        assert_eq!(reviewer.role, Some(AgentRole::Reviewer));
        assert_eq!(reviewer.parent_id, Some(maintainer_id));
        let docker_log = fake_docker_log(&dir);
        assert!(!docker_log.contains("commit maintainer-container"));
        assert!(docker_log.contains(&format!("create --name mai-team-{}", reviewer.id)));
        assert!(docker_log.contains(&format!("mai-team-project-review-{project_id}:/workspace")));
    }

    #[tokio::test]
    async fn deleting_project_reviewer_cleans_review_worktree() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        maintainer.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &maintainer).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        let reviewer = runtime
            .spawn_project_reviewer_agent(project_id)
            .await
            .expect("spawn reviewer");
        let reviewer_id = reviewer.id;

        runtime
            .delete_agent(reviewer_id)
            .await
            .expect("delete reviewer");

        let docker_log = fake_docker_log(&dir);
        assert!(docker_log.contains(&format!("/workspace/reviews/{reviewer_id}")));
        assert!(docker_log.contains("git -C /workspace/repo worktree prune"));
    }

    #[tokio::test]
    async fn project_reviewer_initial_message_uses_latest_extra_prompt() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, Some("maintainer-container"));
        agent.project_id = Some(project_id);
        agent.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &agent).await;
        let mut project = ready_test_project_summary(project_id, agent_id, "account-1");
        project.reviewer_extra_prompt = Some("old prompt".to_string());
        store.save_project(&project).await.expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        runtime
            .update_project(
                project_id,
                UpdateProjectRequest {
                    reviewer_extra_prompt: Some("new prompt".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("update project");
        let message = runtime
            .project_reviewer_initial_message(project_id, reviewer_id, None)
            .await
            .expect("message");

        assert!(message.contains("new prompt"));
        assert!(!message.contains("old prompt"));
        assert!(message.contains("Target pull request: none."));
    }

    #[tokio::test]
    async fn project_reviewer_initial_message_can_target_pr() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let mut agent = test_agent_summary(reviewer_id, None);
        agent.project_id = Some(project_id);
        agent.role = Some(AgentRole::Reviewer);
        save_agent_with_session(&store, &agent).await;
        let project = ready_test_project_summary(project_id, reviewer_id, "account-1");
        store.save_project(&project).await.expect("save project");
        let runtime = test_runtime(&dir, store).await;

        let message = runtime
            .project_reviewer_initial_message(project_id, reviewer_id, Some(42))
            .await
            .expect("message");

        assert!(message.contains("review PR #42 only"));
        assert!(message.contains("select-pr --target-pr 42"));
    }

    #[tokio::test]
    async fn auto_review_refreshes_project_skills_from_synced_default_branch() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .upsert_git_account(GitAccountRequest {
                id: Some("account-1".to_string()),
                label: "GitHub".to_string(),
                token: Some("secret-token".to_string()),
                is_default: true,
                ..Default::default()
            })
            .await
            .expect("save account");
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        maintainer.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &maintainer).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        write_project_skill(
            &runtime,
            project_id,
            "review-default-branch",
            "Old review skill.",
            "Old review body.",
        );
        write_workspace_project_skill(
            &dir,
            ".claude/skills",
            "review-default-branch",
            "New review skill.",
            "New review body.",
        );

        runtime
            .sync_project_review_repo(project_id)
            .await
            .expect("sync review repo");
        runtime
            .refresh_project_skills_from_review_workspace(project_id)
            .await
            .expect("refresh review skills");

        let response = runtime
            .project_skills_from_cache(project_id)
            .await
            .expect("skills");
        let skill = response
            .skills
            .iter()
            .find(|skill| skill.name == "review-default-branch")
            .expect("review skill");
        assert_eq!(skill.description, "New review skill.");
        assert_eq!(
            fs::read_to_string(&skill.path).expect("skill body"),
            "---\nname: review-default-branch\ndescription: New review skill.\n---\nNew review body."
        );
        assert!(fake_docker_log(&dir).contains("review-sync"));
    }

    #[tokio::test]
    async fn project_reviewer_instructions_include_extra_prompt_project_skill() {
        let (base_url, requests) = start_mock_responses(vec![json!({
            "id": "review-skill",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "{\"outcome\":\"no_eligible_pr\",\"pr\":null,\"summary\":\"No eligible pull request found.\",\"error\":null}" }]
            }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
        })])
        .await;
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![compact_test_provider(base_url)],
                default_provider_id: Some("mock".to_string()),
            })
            .await
            .expect("save providers");
        let project_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let project = ProjectSummary {
            reviewer_extra_prompt: Some("用中文评论, review-single-pr".to_string()),
            ..ready_test_project_summary(project_id, reviewer_id, "account-1")
        };
        store.save_project(&project).await.expect("save project");
        let mut reviewer = test_agent_summary(reviewer_id, Some("container-1"));
        reviewer.project_id = Some(project_id);
        reviewer.role = Some(AgentRole::Reviewer);
        let session_id = Uuid::new_v4();
        store.save_agent(&reviewer, None).await.expect("save agent");
        save_test_session(&store, reviewer_id, session_id).await;
        let system_skill_dir = dir
            .path()
            .join("system-skills")
            .join("reviewer-agent-review-pr");
        fs::create_dir_all(&system_skill_dir).expect("mkdir system skill");
        fs::write(
            system_skill_dir.join("SKILL.md"),
            "---\nname: reviewer-agent-review-pr\ndescription: Review one PR.\n---\nReviewer system body.",
        )
        .expect("write system skill");
        let runtime = AgentRuntime::new(
            DockerClient::new_with_binary("unused", fake_docker_path(&dir)),
            ModelClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                system_skills_root: Some(dir.path().join("system-skills")),
                ..test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE)
            },
        )
        .await
        .expect("runtime");
        let agent = runtime.agent(reviewer_id).await.expect("agent");
        *agent.container.write().await = Some(ContainerHandle {
            id: "container-1".to_string(),
            name: "container-1".to_string(),
            image: "unused".to_string(),
        });
        runtime.state.project_mcp_managers.write().await.insert(
            project_id,
            Arc::new(McpAgentManager::from_tools_for_test(Vec::new())),
        );
        write_project_skill(
            &runtime,
            project_id,
            "review-single-pr",
            "Review exactly one pull request with Chinese comments.",
            "Review single PR body.",
        );
        write_workspace_project_skill(
            &dir,
            ".claude/skills",
            "review-single-pr",
            "Review exactly one pull request with Chinese comments.",
            "Review single PR body.",
        );
        let message = runtime
            .project_reviewer_initial_message(project_id, reviewer_id, None)
            .await
            .expect("message");
        runtime
            .run_turn_inner(
                reviewer_id,
                session_id,
                Uuid::new_v4(),
                message,
                vec!["reviewer-agent-review-pr".to_string()],
                CancellationToken::new(),
            )
            .await
            .expect("turn");

        let requests = requests.lock().await.clone();
        let request_text = serde_json::to_string(&requests[0]).expect("request json");
        assert!(request_text.contains("用中文评论, review-single-pr"));
        assert!(request_text.contains("$review-single-pr"));
        assert!(request_text.contains("Review exactly one pull request with Chinese comments."));
        assert!(
            request_text.contains("/workspace/.mai-team/skills/project/review-single-pr/SKILL.md")
        );
        assert!(
            request_text
                .contains("/workspace/.mai-team/skills/system/reviewer-agent-review-pr/SKILL.md")
        );
        assert!(!request_text.contains("/workspace/repo/.claude/skills/review-single-pr/SKILL.md"));
        assert!(request_text.contains("<name>reviewer-agent-review-pr</name>"));
        assert!(request_text.contains("<name>review-single-pr</name>"));
        let docker_log = fake_docker_log(&dir);
        assert!(
            docker_log.contains("cp")
                && docker_log
                    .contains("/workspace/.mai-team/skills/system/reviewer-agent-review-pr")
        );
        assert!(
            docker_log.contains("cp")
                && docker_log.contains("/workspace/.mai-team/skills/project/review-single-pr")
        );
    }

    #[tokio::test]
    async fn project_detail_includes_recent_review_runs() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, None);
        agent.project_id = Some(project_id);
        agent.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &agent).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                agent_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let run_id = Uuid::new_v4();
        let started_at = now();
        store
            .save_project_review_run(&ProjectReviewRunDetail {
                summary: ProjectReviewRunSummary {
                    id: run_id,
                    project_id,
                    reviewer_agent_id: Some(Uuid::new_v4()),
                    turn_id: Some(Uuid::new_v4()),
                    started_at,
                    finished_at: Some(started_at + TimeDelta::minutes(1)),
                    status: ProjectReviewRunStatus::Completed,
                    outcome: Some(ProjectReviewOutcome::ReviewSubmitted),
                    pr: Some(7),
                    summary: Some("submitted review".to_string()),
                    error: None,
                },
                messages: vec![AgentMessage {
                    role: MessageRole::Assistant,
                    content: "review complete".to_string(),
                    created_at: started_at,
                }],
                events: Vec::new(),
            })
            .await
            .expect("save run");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        let detail = runtime
            .get_project(project_id, None, None)
            .await
            .expect("detail");
        assert_eq!(detail.review_runs.len(), 1);
        assert_eq!(detail.review_runs[0].id, run_id);
        assert_eq!(detail.review_runs[0].pr, Some(7));
        let run = runtime
            .get_project_review_run(project_id, run_id)
            .await
            .expect("run detail");
        assert_eq!(run.messages[0].content, "review complete");
    }

    #[tokio::test]
    async fn finishing_project_review_run_captures_reviewer_snapshot() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, None);
        maintainer.project_id = Some(project_id);
        maintainer.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &maintainer).await;
        let mut reviewer = test_agent_summary(reviewer_id, None);
        reviewer.project_id = Some(project_id);
        reviewer.parent_id = Some(maintainer_id);
        reviewer.role = Some(AgentRole::Reviewer);
        reviewer.status = AgentStatus::Completed;
        store
            .save_agent(&reviewer, None)
            .await
            .expect("save reviewer");
        store
            .save_agent_session(
                reviewer_id,
                &AgentSessionSummary {
                    id: session_id,
                    title: "Review".to_string(),
                    created_at: now(),
                    updated_at: now(),
                    message_count: 0,
                },
            )
            .await
            .expect("save reviewer session");
        store
            .append_agent_message(
                reviewer_id,
                session_id,
                0,
                &AgentMessage {
                    role: MessageRole::Assistant,
                    content: "snapshot summary".to_string(),
                    created_at: now(),
                },
            )
            .await
            .expect("message");
        store
            .save_project(&ready_test_project_summary(
                project_id,
                maintainer_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        store
            .save_project_review_run(&ProjectReviewRunDetail {
                summary: ProjectReviewRunSummary {
                    id: run_id,
                    project_id,
                    reviewer_agent_id: Some(reviewer_id),
                    turn_id: Some(turn_id),
                    started_at: now(),
                    finished_at: None,
                    status: ProjectReviewRunStatus::Running,
                    outcome: None,
                    pr: None,
                    summary: None,
                    error: None,
                },
                messages: Vec::new(),
                events: Vec::new(),
            })
            .await
            .expect("save run");
        runtime
            .events
            .publish(ServiceEventKind::TurnCompleted {
                agent_id: reviewer_id,
                session_id: Some(session_id),
                turn_id,
                status: TurnStatus::Completed,
            })
            .await;

        projects::review::runs::finish_project_review_run(
            &runtime.deps.store,
            runtime.as_ref(),
            FinishReviewRun {
                run_id,
                project_id,
                reviewer_agent_id: Some(reviewer_id),
                turn_id: Some(turn_id),
                status: ProjectReviewRunStatus::Completed,
                outcome: Some(ProjectReviewOutcome::ReviewSubmitted),
                pr: Some(12),
                summary_text: Some("submitted".to_string()),
                error: None,
            },
        )
        .await
        .expect("finish");

        let run = runtime
            .get_project_review_run(project_id, run_id)
            .await
            .expect("run");
        assert_eq!(run.summary.pr, Some(12));
        assert_eq!(run.messages[0].content, "snapshot summary");
        assert!(run.events.iter().any(|event| {
            matches!(
                event.kind,
                ServiceEventKind::TurnCompleted { agent_id, .. } if agent_id == reviewer_id
            )
        }));
    }

    #[tokio::test]
    async fn project_review_retention_cleanup_preserves_repo_path() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, None);
        agent.project_id = Some(project_id);
        agent.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &agent).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                agent_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        runtime
            .cleanup_project_review_workspace_history(
                project_id,
                now() - TimeDelta::days(PROJECT_REVIEW_HISTORY_RETENTION_DAYS),
            )
            .await
            .expect("cleanup");

        let docker_log = fake_docker_log(&dir);
        assert!(docker_log.contains("git -C /workspace/repo worktree prune"));
        assert!(docker_log.contains("/workspace/reviews"));
        assert!(!docker_log.contains("rm -rf /workspace/repo"));
    }

    #[tokio::test]
    async fn delete_project_removes_project_mcp_manager() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, Some("maintainer-container"));
        agent.project_id = Some(project_id);
        save_agent_with_session(&store, &agent).await;
        store
            .save_project(&ready_test_project_summary(
                project_id,
                agent_id,
                "account-1",
            ))
            .await
            .expect("save project");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;
        runtime.state.project_mcp_managers.write().await.insert(
            project_id,
            Arc::new(McpAgentManager::from_tools_for_test(vec![test_mcp_tool(
                "github", "get_me",
            )])),
        );

        runtime.delete_project(project_id).await.expect("delete");

        assert!(
            !runtime
                .state
                .project_mcp_managers
                .read()
                .await
                .contains_key(&project_id)
        );
    }

    #[tokio::test]
    async fn project_workspace_setup_failure_marks_project_failed() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        store
            .upsert_git_account(GitAccountRequest {
                id: Some("account-1".to_string()),
                label: "GitHub".to_string(),
                token: Some("secret-token".to_string()),
                is_default: true,
                ..Default::default()
            })
            .await
            .expect("save account");
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let mut agent = test_agent_summary(agent_id, None);
        agent.status = AgentStatus::Created;
        agent.project_id = Some(project_id);
        agent.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &agent).await;
        let project = test_project_summary(project_id, agent_id, "account-1");
        store.save_project(&project).await.expect("save project");
        let runtime = AgentRuntime::new(
            DockerClient::new_with_binary("unused-agent", failing_docker_path(&dir)),
            ModelClient::new(),
            Arc::clone(&store),
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
        )
        .await
        .expect("runtime");

        runtime
            .start_project_workspace(project_id, agent_id)
            .await
            .expect("setup handles failure");

        let detail = runtime
            .get_project(project_id, None, None)
            .await
            .expect("detail");
        assert_eq!(detail.summary.status, ProjectStatus::Failed);
        assert_eq!(detail.summary.clone_status, ProjectCloneStatus::Failed);
        assert_eq!(detail.maintainer_agent.summary.status, AgentStatus::Failed);
        assert!(
            detail
                .summary
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("container startup failed")
        );
        assert!(!fake_docker_log(&dir).contains("exec -w / -e MAI_GITHUB_INSTALLATION_TOKEN"));
    }

    #[tokio::test]
    async fn project_sidecar_is_removed_when_project_is_deleted() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        store
            .upsert_git_account(GitAccountRequest {
                id: Some("account-1".to_string()),
                label: "GitHub".to_string(),
                token: Some("secret-token".to_string()),
                is_default: true,
                ..Default::default()
            })
            .await
            .expect("save account");
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let agent_container_id = format!("mai-team-{agent_id}");
        let mut agent = test_agent_summary(agent_id, Some(&agent_container_id));
        agent.project_id = Some(project_id);
        agent.role = Some(AgentRole::Planner);
        save_agent_with_session(&store, &agent).await;
        let project = test_project_summary(project_id, agent_id, "account-1");
        store.save_project(&project).await.expect("save project");
        let runtime = test_runtime_with_sidecar_image_and_git(
            &dir,
            Arc::clone(&store),
            "ghcr.io/example/mai-team-sidecar:test",
        )
        .await;

        runtime
            .clone_project_repository(project_id, agent_id)
            .await
            .expect("clone");
        runtime
            .delete_project(project_id)
            .await
            .expect("delete project");

        let docker_log = fake_docker_log(&dir);
        assert!(docker_log.contains(&format!(
            "create --name mai-team-project-sidecar-{project_id}"
        )));
        assert!(docker_log.contains("rm -f created-container"));
        assert!(docker_log.contains(&format!("rm -f mai-team-{agent_id}")));
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
            project_id: None,
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
            ModelClient::new(),
            Arc::clone(&store),
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
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
            project_id: None,
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
            ModelClient::new(),
            Arc::clone(&store),
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
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
            project_id: None,
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
            ModelClient::new(),
            store,
            test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
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

        let preview = turn::tools::trace_preview_value(&value, 1_000);

        assert!(preview.contains("echo ok"));
        assert!(preview.contains("<redacted>"));
        assert!(!preview.contains("secret"));
        assert!(!preview.contains(&"a".repeat(120)));
    }
}
