use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mai_agents::AgentProfilesManager;
use mai_docker::{ContainerHandle, DockerClient};
use mai_mcp::McpAgentManager;
#[cfg(test)]
use mai_mcp::McpTool;
use mai_model::{ModelClient, ModelTurnState};
use mai_protocol::*;
#[cfg(test)]
use mai_protocol::{MessageRole, ModelContentItem};
use mai_skills::{SkillInjections, SkillsManager};
use mai_store::{AgentLogFilter, ConfigStore, ProviderSelection, ToolTraceFilter};
#[cfg(test)]
use mai_tools::build_tool_definitions_with_filter;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio::time::{Duration, Instant, sleep};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod agents;
mod deps;
mod events;
pub mod github;
mod instructions;
mod projects;
mod state;
mod tasks;
mod tools;
mod turn;

use agents::AgentResourceBroker;
use deps::RuntimeDeps;
use events::{RECENT_EVENT_LIMIT, RuntimeEvents};
use github::{
    DEFAULT_GITHUB_API_BASE_URL, DirectGithubAppBackend, GITHUB_HTTP_TIMEOUT_SECS, GithubAppBackend,
};
use instructions::{CONTAINER_SKILLS_ROOT, ContainerSkillPaths};
use projects::review::ProjectReviewCycleResult;
use projects::review::pool::{ProjectReviewPoolEnqueueSummary, ProjectReviewSignalInput};
use projects::review::runs::FinishReviewRun;
use projects::review::state::ReviewStateUpdate;
#[cfg(test)]
use projects::skills::PROJECT_SKILLS_CACHE_DIR;
use projects::skills::ProjectSkillSourceDir;
use projects::workspace::ProjectWorkspaceManager;
use state::{AgentRecord, AgentSessionRecord, ProjectRecord, RuntimeState, TaskRecord};
use turn::tools::ToolExecution;

const AUTO_COMPACT_THRESHOLD_PERCENT: u64 = 90;
const PROJECT_REVIEW_RUN_LIST_LIMIT: usize = 50;
const PROJECT_REVIEW_SNAPSHOT_MESSAGE_LIMIT: usize = 40;
const PROJECT_REVIEW_SNAPSHOT_EVENT_LIMIT: usize = 80;
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

#[derive(Debug, Clone)]
pub struct ProjectReviewQueueRequest {
    pub project_id: ProjectId,
    pub pr: u64,
    pub head_sha: Option<String>,
    pub delivery_id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectReviewQueueSummary {
    pub queued: Vec<u64>,
    pub deduped: Vec<u64>,
    pub ignored: Vec<u64>,
}

impl From<ProjectReviewPoolEnqueueSummary> for ProjectReviewQueueSummary {
    fn from(value: ProjectReviewPoolEnqueueSummary) -> Self {
        Self {
            queued: value.queued,
            deduped: value.deduped,
            ignored: value.ignored,
        }
    }
}

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
    pub projects_root: PathBuf,
    pub cache_root: PathBuf,
    pub artifact_files_root: PathBuf,
    pub sidecar_image: String,
    pub github_api_base_url: Option<String>,
    pub git_binary: Option<String>,
    pub system_skills_root: Option<PathBuf>,
    pub system_agents_root: Option<PathBuf>,
}

pub struct AgentRuntime {
    deps: RuntimeDeps,
    state: RuntimeState,
    events: RuntimeEvents,
    projects_root: PathBuf,
    cache_root: PathBuf,
    artifact_files_root: PathBuf,
    sidecar_image: String,
    github_api_base_url: String,
    git_binary: String,
    workspace_manager: projects::workspace::LocalProjectWorkspaceManager,
}

struct ResolvedAgentModel {
    preference: AgentModelPreference,
    effective: ResolvedAgentModelPreference,
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
            projects.insert(summary.id, Arc::new(ProjectRecord::new(summary)));
        }
        let sidecar_image = runtime_sidecar_image(config.sidecar_image);
        let github_api_base_url = config
            .github_api_base_url
            .as_deref()
            .unwrap_or(DEFAULT_GITHUB_API_BASE_URL)
            .to_string();
        let git_binary = config
            .git_binary
            .clone()
            .unwrap_or_else(|| "git".to_string());
        let projects_root = config.projects_root;
        let workspace_manager = projects::workspace::LocalProjectWorkspaceManager::new(
            git_binary.clone(),
            projects_root.clone(),
        );
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
            projects_root,
            cache_root: config.cache_root,
            artifact_files_root: config.artifact_files_root,
            sidecar_image,
            github_api_base_url,
            git_binary,
            workspace_manager,
        });
        let cleanup_runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            projects::review::cleanup::run_project_review_cleanup_loop(&cleanup_runtime).await;
        });
        runtime.reconcile_project_workspaces().await?;
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

        let repo_path = self
            .workspace_manager
            .agent_clone_path(project_id, summary.maintainer_agent_id);
        let existing = projects::skills::detect_existing_dirs_in_host_repo(&repo_path);
        self.refresh_project_skill_cache(project_id, &existing)
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
        tasks::list_tasks(&self.state).await
    }

    pub async fn list_environments(&self) -> Vec<EnvironmentSummary> {
        tasks::list_environments(&self.state).await
    }

    pub async fn list_projects(&self) -> Vec<ProjectSummary> {
        projects::service::list_projects(&self.state).await
    }

    pub async fn list_git_accounts(&self) -> Result<GitAccountsResponse> {
        github::list_git_accounts(&self.deps.git_accounts).await
    }

    pub async fn save_git_account(
        self: &Arc<Self>,
        request: GitAccountRequest,
    ) -> Result<GitAccountResponse> {
        github::save_git_account(&self.deps.git_accounts, request).await
    }

    pub async fn verify_git_account(&self, account_id: &str) -> Result<GitAccountSummary> {
        github::verify_git_account(&self.deps.git_accounts, account_id).await
    }

    pub async fn delete_git_account(&self, account_id: &str) -> Result<GitAccountsResponse> {
        github::delete_git_account(&self.deps.git_accounts, account_id).await
    }

    pub async fn set_default_git_account(&self, account_id: &str) -> Result<GitAccountsResponse> {
        github::set_default_git_account(&self.deps.git_accounts, account_id).await
    }

    pub async fn list_git_account_repositories(
        &self,
        account_id: &str,
    ) -> Result<GithubRepositoriesResponse> {
        github::list_git_account_repositories(&self.deps.git_accounts, account_id).await
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
        github::list_git_account_repository_packages(
            &self.deps.git_accounts,
            account_id,
            owner,
            repo,
        )
        .await
    }

    pub async fn list_github_installation_repository_packages(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
    ) -> Result<RepositoryPackagesResponse> {
        github::list_github_installation_repository_packages(
            self.deps.github_backend.as_ref(),
            &self.deps.github_http,
            &self.github_api_base_url,
            installation_id,
            owner,
            repo,
        )
        .await
    }

    pub async fn github_app_settings(&self) -> Result<GithubAppSettingsResponse> {
        github::github_app_settings(self.deps.github_backend.as_ref()).await
    }

    pub async fn save_github_app_settings(
        &self,
        request: GithubAppSettingsRequest,
    ) -> Result<GithubAppSettingsResponse> {
        github::save_github_app_settings(self.deps.github_backend.as_ref(), request).await
    }

    pub async fn start_github_app_manifest(
        &self,
        request: GithubAppManifestStartRequest,
    ) -> Result<GithubAppManifestStartResponse> {
        github::start_github_app_manifest(self.deps.github_backend.as_ref(), request).await
    }

    pub async fn complete_github_app_manifest(
        &self,
        code: &str,
        state: &str,
    ) -> Result<GithubAppSettingsResponse> {
        github::complete_github_app_manifest(self.deps.github_backend.as_ref(), code, state).await
    }

    pub async fn list_github_installations(&self) -> Result<GithubInstallationsResponse> {
        github::list_github_installations(self.deps.github_backend.as_ref()).await
    }

    pub async fn refresh_github_installations(&self) -> Result<GithubInstallationsResponse> {
        github::refresh_github_installations(self.deps.github_backend.as_ref()).await
    }

    pub async fn list_github_repositories(
        &self,
        installation_id: u64,
    ) -> Result<GithubRepositoriesResponse> {
        github::list_github_repositories(self.deps.github_backend.as_ref(), installation_id).await
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

    pub async fn ensure_default_environment(
        self: &Arc<Self>,
    ) -> Result<Option<EnvironmentSummary>> {
        let environments = self.list_environments().await;
        if let Some(environment) = environments.first() {
            return Ok(Some(environment.clone()));
        }
        match self
            .create_environment(
                tasks::DEFAULT_ENVIRONMENT_NAME.to_string(),
                Some(self.deps.docker.image().to_string()),
            )
            .await
        {
            Ok(environment) => Ok(Some(environment)),
            Err(RuntimeError::Store(mai_store::StoreError::InvalidConfig(_))) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn create_environment(
        self: &Arc<Self>,
        name: String,
        docker_image: Option<String>,
    ) -> Result<EnvironmentSummary> {
        tasks::create_environment(
            &self.state,
            self,
            tasks::CreateEnvironmentInput { name, docker_image },
        )
        .await
    }

    pub async fn get_environment(
        self: &Arc<Self>,
        environment_id: EnvironmentId,
        session_id: Option<SessionId>,
    ) -> Result<EnvironmentDetail> {
        tasks::get_environment(&self.state, self, environment_id, session_id).await
    }

    pub async fn create_environment_conversation(
        self: &Arc<Self>,
        environment_id: EnvironmentId,
    ) -> Result<AgentSessionSummary> {
        tasks::create_environment_conversation(&self.state, self, environment_id).await
    }

    pub async fn send_environment_message(
        self: &Arc<Self>,
        environment_id: EnvironmentId,
        session_id: SessionId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        tasks::send_environment_message(
            &self.state,
            self,
            environment_id,
            session_id,
            message,
            skill_mentions,
        )
        .await
    }

    pub async fn create_task(
        self: &Arc<Self>,
        title: Option<String>,
        initial_message: Option<String>,
        docker_image: Option<String>,
    ) -> Result<TaskSummary> {
        tasks::create_task(
            &self.state,
            self,
            tasks::CreateTaskInput {
                title,
                initial_message,
                docker_image,
            },
        )
        .await
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
        tasks::update_task_title(&self.state, self.as_ref(), task_id, new_title).await
    }

    pub async fn get_task(
        &self,
        task_id: TaskId,
        selected_agent_id: Option<AgentId>,
    ) -> Result<TaskDetail> {
        tasks::get_task(&self.state, self, task_id, selected_agent_id).await
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
            projects::review::cleanup::PROJECT_REVIEW_HISTORY_RETENTION_DAYS,
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
        projects::service::create_project(self, request).await
    }

    pub async fn update_project(
        self: &Arc<Self>,
        project_id: ProjectId,
        request: UpdateProjectRequest,
    ) -> Result<ProjectSummary> {
        projects::service::update_project(&self.state, self, project_id, request).await
    }

    pub async fn delete_project(self: &Arc<Self>, project_id: ProjectId) -> Result<()> {
        projects::service::delete_project(&self.state, self, project_id).await
    }

    pub async fn cancel_project(self: &Arc<Self>, project_id: ProjectId) -> Result<()> {
        projects::service::cancel_project(&self.state, self, project_id).await
    }

    pub async fn send_project_message(
        self: &Arc<Self>,
        project_id: ProjectId,
        request: SendMessageRequest,
    ) -> Result<TurnId> {
        projects::service::send_project_message(&self.state, self, project_id, request).await
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

    pub async fn enqueue_project_review(
        self: &Arc<Self>,
        request: ProjectReviewQueueRequest,
    ) -> Result<ProjectReviewQueueSummary> {
        let project_id = request.project_id;
        let project = self.project(project_id).await?;
        {
            let summary = project.summary.read().await;
            if !summary.auto_review_enabled {
                return Ok(ProjectReviewPoolEnqueueSummary {
                    ignored: vec![request.pr],
                    ..Default::default()
                }
                .into());
            }
        }
        let summary = {
            let mut pool = project.review_pool.lock().await;
            pool.enqueue_many([ProjectReviewSignalInput {
                pr: request.pr,
                head_sha: request.head_sha,
                delivery_id: request.delivery_id.clone(),
                reason: request.reason.clone(),
            }])
        };
        if !summary.queued.is_empty() || !summary.deduped.is_empty() {
            self.events
                .publish(ServiceEventKind::ProjectReviewQueued {
                    project_id,
                    delivery_id: request.delivery_id.clone().unwrap_or_default(),
                    pr: request.pr,
                    reason: request.reason.clone(),
                })
                .await;
            project.review_notify.notify_one();
            if let Err(err) = self.start_project_review_loop_if_ready(project_id).await {
                tracing::warn!(
                    project_id = %project_id,
                    "failed to start project review loop after queueing PR signal: {err}"
                );
            }
        }
        tracing::info!(
            project_id = %project_id,
            pr = request.pr,
            delivery_id = request.delivery_id.as_deref().unwrap_or_default(),
            reason = %request.reason,
            queued = ?summary.queued,
            deduped = ?summary.deduped,
            ignored = ?summary.ignored,
            "queued project review signal"
        );
        Ok(summary.into())
    }

    pub async fn send_task_message(
        self: &Arc<Self>,
        task_id: TaskId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        tasks::send_task_message(&self.state, self, task_id, message, skill_mentions).await
    }

    pub async fn approve_task_plan(self: &Arc<Self>, task_id: TaskId) -> Result<TaskSummary> {
        tasks::approve_task_plan(&self.state, self, task_id).await
    }

    pub async fn request_plan_revision(
        self: &Arc<Self>,
        task_id: TaskId,
        feedback: String,
    ) -> Result<TaskSummary> {
        tasks::request_plan_revision(&self.state, self, task_id, feedback).await
    }

    pub async fn create_agent(
        self: &Arc<Self>,
        request: CreateAgentRequest,
    ) -> Result<AgentSummary> {
        self.create_agent_with_container_source(
            request,
            agents::ContainerSource::FreshImage,
            None,
            None,
            None,
        )
        .await
    }

    async fn create_agent_with_container_source(
        self: &Arc<Self>,
        request: CreateAgentRequest,
        container_source: agents::ContainerSource,
        task_id: Option<TaskId>,
        project_id: Option<ProjectId>,
        role: Option<AgentRole>,
    ) -> Result<AgentSummary> {
        let agent = agents::create_agent_record(
            self.as_ref(),
            request,
            agents::CreateAgentRecordContext {
                task_id,
                project_id,
                role,
            },
        )
        .await?;
        let container_source = self
            .agent_container_source_for_project(
                agent.summary.read().await.id,
                project_id,
                container_source,
            )
            .await?;

        match agents::ensure_agent_container_with_source(
            self.as_ref(),
            &agent,
            AgentStatus::Idle,
            &container_source,
            None,
        )
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

    async fn agent_container_source_for_project(
        &self,
        agent_id: AgentId,
        project_id: Option<ProjectId>,
        source: agents::ContainerSource,
    ) -> Result<agents::ContainerSource> {
        let Some(project_id) = project_id else {
            return Ok(source);
        };
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        if summary.status != ProjectStatus::Ready
            && summary.clone_status != ProjectCloneStatus::Ready
        {
            return Ok(source);
        }
        let clone = self
            .workspace_manager
            .agent_clone_path(summary.id, agent_id);
        let clone = if clone.exists() {
            clone
        } else {
            self.workspace_manager
                .prepare_agent_clone(
                    &summary,
                    agent_id,
                    projects::workspace::CloneSeed::DefaultBranch,
                )
                .await?
                .path
        };
        Ok(match source {
            agents::ContainerSource::FreshImage | agents::ContainerSource::ProjectClone { .. } => {
                agents::ContainerSource::ProjectClone {
                    clone_path: clone.to_string_lossy().to_string(),
                }
            }
            agents::ContainerSource::CloneFrom {
                parent_container_id,
                docker_image,
                workspace_volume,
                repo_mount: _,
            } => agents::ContainerSource::CloneFrom {
                parent_container_id,
                docker_image,
                workspace_volume,
                repo_mount: Some(clone.to_string_lossy().to_string()),
            },
        })
    }

    pub async fn list_agents(&self) -> Vec<AgentSummary> {
        let agents = self.state.agents.read().await.values().cloned().collect();
        agents::list_agents(agents).await
    }

    async fn reconcile_project_workspaces(&self) -> Result<()> {
        let projects = self.list_projects().await;
        let agents = self.list_agents().await;
        let report = self.workspace_manager.reconcile(&projects, &agents).await?;
        if !report.orphan_clones_removed.is_empty() {
            tracing::info!(
                count = report.orphan_clones_removed.len(),
                "removed orphan project clone directories during startup reconcile"
            );
        }
        if !report.orphan_project_dirs_archived.is_empty() {
            tracing::info!(
                count = report.orphan_project_dirs_archived.len(),
                "archived orphan project directories during startup reconcile"
            );
        }
        if !report.legacy_worktree_dirs_archived.is_empty() {
            tracing::info!(
                count = report.legacy_worktree_dirs_archived.len(),
                "archived legacy project worktree directories during startup reconcile"
            );
        }
        if !report.missing_repo_caches.is_empty() {
            tracing::warn!(
                count = report.missing_repo_caches.len(),
                "found projects with missing repository caches during startup reconcile"
            );
            for project_id in &report.missing_repo_caches {
                let repo_cache_path = self.workspace_manager.repo_cache_path(*project_id);
                let project_dir_exists = repo_cache_path
                    .parent()
                    .is_some_and(|project_dir| project_dir.exists());
                if !project_dir_exists {
                    continue;
                }
                self.set_project_clone_result(
                    *project_id,
                    ProjectStatus::Failed,
                    ProjectCloneStatus::Failed,
                    Some("project repository cache is missing after startup reconcile".to_string()),
                )
                .await?;
            }
        }
        if !report.missing_agent_clones.is_empty() {
            tracing::warn!(
                count = report.missing_agent_clones.len(),
                "found project agents with missing clones during startup reconcile"
            );
            for agent_id in &report.missing_agent_clones {
                let Some(agent_summary) = agents.iter().find(|agent| agent.id == *agent_id) else {
                    continue;
                };
                let Some(project_id) = agent_summary.project_id else {
                    continue;
                };
                if report.missing_repo_caches.contains(&project_id) {
                    continue;
                }
                let Some(project) = projects.iter().find(|project| project.id == project_id) else {
                    continue;
                };
                if let Err(err) = self
                    .workspace_manager
                    .prepare_agent_clone(
                        project,
                        *agent_id,
                        projects::workspace::CloneSeed::DefaultBranch,
                    )
                    .await
                {
                    let agent = self.agent(*agent_id).await?;
                    self.set_status(
                        &agent,
                        AgentStatus::Failed,
                        Some(format!(
                            "project clone could not be restored after startup reconcile: {err}"
                        )),
                    )
                    .await?;
                }
            }
        }
        if !report.invalid_clone_dirs.is_empty() {
            tracing::warn!(
                count = report.invalid_clone_dirs.len(),
                "found invalid project clone directories during startup reconcile"
            );
        }
        Ok(())
    }

    pub async fn update_agent(
        &self,
        agent_id: AgentId,
        request: UpdateAgentRequest,
    ) -> Result<AgentSummary> {
        agents::update_agent(self, agent_id, request).await
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
        agents::tool_trace(self, agent_id, session_id, call_id).await
    }

    pub async fn tool_output_artifact(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        call_id: String,
        artifact_id: String,
    ) -> Result<(ToolOutputArtifactInfo, PathBuf)> {
        agents::tool_output_artifact(self, agent_id, session_id, call_id, artifact_id).await
    }

    pub async fn agent_logs(
        &self,
        agent_id: AgentId,
        filter: AgentLogFilter,
    ) -> Result<AgentLogsResponse> {
        agents::agent_logs(self, agent_id, filter).await
    }

    pub async fn tool_traces(
        &self,
        agent_id: AgentId,
        filter: ToolTraceFilter,
    ) -> Result<ToolTraceListResponse> {
        agents::tool_traces(self, agent_id, filter).await
    }

    pub async fn send_message(
        self: &Arc<Self>,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        agents::send_message(
            self.as_ref(),
            self,
            agent_id,
            session_id,
            message,
            skill_mentions,
        )
        .await
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
        agents::spawn_turn(
            self,
            agent,
            agent_id,
            session_id,
            turn_id,
            message,
            skill_mentions,
        );
    }

    pub async fn cancel_agent(self: &Arc<Self>, agent_id: AgentId) -> Result<()> {
        agents::cancel_agent(self, agent_id).await
    }

    pub async fn cancel_agent_turn(
        self: &Arc<Self>,
        agent_id: AgentId,
        turn_id: TurnId,
    ) -> Result<()> {
        agents::cancel_agent_turn(self, agent_id, turn_id).await
    }

    pub async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
        agents::delete_agent(self, agent_id).await
    }

    async fn close_agent(&self, agent_id: AgentId) -> Result<AgentStatus> {
        agents::close_agent(self, agent_id).await
    }

    async fn resume_agent(&self, agent_id: AgentId) -> Result<AgentSummary> {
        agents::resume_agent(self, agent_id).await
    }

    pub async fn cancel_task(self: &Arc<Self>, task_id: TaskId) -> Result<()> {
        tasks::cancel_task(&self.state, self, task_id).await
    }

    pub async fn delete_task(self: &Arc<Self>, task_id: TaskId) -> Result<()> {
        tasks::delete_task(&self.state, self, task_id).await
    }

    pub async fn upload_file(
        &self,
        agent_id: AgentId,
        path: String,
        content_base64: String,
    ) -> Result<usize> {
        agents::upload_file(self, agent_id, path, content_base64).await
    }

    pub async fn download_file_tar(&self, agent_id: AgentId, path: String) -> Result<Vec<u8>> {
        agents::download_file_tar(self, agent_id, path).await
    }

    pub async fn save_artifact(
        self: &Arc<Self>,
        agent_id: AgentId,
        path: String,
        display_name: Option<String>,
    ) -> Result<ArtifactInfo> {
        tasks::save_artifact(&self.state, self.as_ref(), agent_id, path, display_name).await
    }

    pub fn artifact_file_path(&self, info: &ArtifactInfo) -> PathBuf {
        tasks::artifact_file_path(&self.artifact_files_root, info)
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
            turn::orchestrator::TurnRequest {
                agent_id,
                session_id,
                turn_id,
                message,
                skill_mentions,
                cancellation_token,
            },
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
            turn::orchestrator::TurnRequest {
                agent_id,
                session_id,
                turn_id,
                message,
                skill_mentions,
                cancellation_token,
            },
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
        tasks::save_task_plan(&self.state, self.as_ref(), agent_id, title, markdown).await
    }

    async fn submit_review_result(
        self: &Arc<Self>,
        agent_id: AgentId,
        passed: bool,
        findings: String,
        summary: String,
    ) -> Result<TaskReview> {
        tasks::submit_review_result(
            &self.state,
            self.as_ref(),
            agent_id,
            passed,
            findings,
            summary,
        )
        .await
    }

    fn spawn_task_workflow(self: &Arc<Self>, task_id: TaskId) {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = tasks::run_task_workflow(&runtime.state, &runtime, task_id).await
                && let Ok(task) = runtime.task(task_id).await
            {
                let _ = runtime
                    .set_task_status(&task, TaskStatus::Failed, None, Some(err.to_string()))
                    .await;
            }
        });
    }

    async fn spawn_task_role_agent(
        self: &Arc<Self>,
        parent_agent_id: AgentId,
        role: AgentRole,
        name: Option<String>,
    ) -> Result<AgentSummary> {
        agents::spawn_task_role_agent(self, parent_agent_id, role, name).await
    }

    async fn start_agent_turn(
        self: &Arc<Self>,
        agent_id: AgentId,
        message: String,
    ) -> Result<TurnId> {
        agents::start_agent_turn(self.as_ref(), self, agent_id, message, Vec::new()).await
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
            agents::SendInputRequest {
                target,
                session_id,
                message,
                skill_mentions,
                interrupt,
                cancel_grace: TURN_CANCEL_GRACE,
            },
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
            turn::persistence::AgentLogRecord {
                agent_id,
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                level: "info",
                category: "context",
                message: "context compacted",
                details: json!({
                    "tokens_before": tokens_before,
                    "summary_preview": preview(&summary_text, COMPACT_SUMMARY_PREVIEW_CHARS),
                }),
            },
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
        tasks::task(&self.state, task_id).await
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
        let repo_path = self
            .workspace_manager
            .agent_clone_path(project_id, summary.maintainer_agent_id);
        let existing = projects::skills::detect_existing_dirs_in_host_repo(&repo_path);
        self.refresh_project_skill_cache(project_id, &existing)
            .await
    }

    async fn refresh_project_skills_from_review_workspace(
        &self,
        project_id: ProjectId,
    ) -> Result<()> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let repo_path = self
            .workspace_manager
            .agent_clone_path(project_id, summary.maintainer_agent_id);
        let sources = projects::skills::detect_existing_dirs_in_host_repo(&repo_path);
        self.refresh_project_skill_cache(project_id, &sources).await
    }

    fn project_skill_cache_dir(&self, project_id: ProjectId) -> PathBuf {
        projects::skills::cache_dir(&self.cache_root, project_id)
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

    async fn refresh_project_skill_cache(
        &self,
        project_id: ProjectId,
        sources: &[ProjectSkillSourceDir],
    ) -> Result<()> {
        let lock = self.project_skill_lock(project_id).await;
        projects::skills::refresh_cache(&self.cache_root, &lock, project_id, sources).await
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
        if let Some(manager) = projects::mcp::cached_manager(&self.state, project_id).await {
            return Ok(Some(manager));
        }

        let token = self.project_git_token(project_id).await?;
        if token.is_none() {
            return Ok(None);
        }
        self.events
            .publish(ServiceEventKind::McpServerStatusChanged {
                agent_id,
                server: "project".to_string(),
                status: mai_protocol::McpStartupStatus::Starting,
                error: None,
            })
            .await;
        let manager = projects::mcp::ensure_manager(
            &self.state,
            &self.deps.docker,
            &self.sidecar_image,
            project_id,
            token.as_deref(),
            cancellation_token,
        )
        .await?;
        if let Some(manager) = manager.as_ref() {
            for status in manager.statuses().await {
                let error = status
                    .error
                    .map(|error| redact_secret(&error, token.as_deref().unwrap_or_default()));
                self.events
                    .publish(ServiceEventKind::McpServerStatusChanged {
                        agent_id,
                        server: status.server,
                        status: status.status,
                        error,
                    })
                    .await;
            }
        }
        Ok(manager)
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
        projects::mcp::shutdown_manager(&self.state, project_id).await;
    }

    async fn delete_project_sidecar(&self, project_id: ProjectId) -> Result<Vec<String>> {
        projects::mcp::delete_sidecar(&self.state, &self.deps.docker, project_id).await
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
        let setup_result: Result<()> = async {
            self.set_project_clone_result(
                project_id,
                ProjectStatus::Creating,
                ProjectCloneStatus::Cloning,
                None,
            )
            .await?;
            self.clone_project_repository(project_id, maintainer_agent_id)
                .await?;
            self.set_project_clone_result(
                project_id,
                ProjectStatus::Ready,
                ProjectCloneStatus::Ready,
                None,
            )
            .await?;
            let maintainer = self.agent(maintainer_agent_id).await?;
            let source = self
                .agent_container_source_for_project(
                    maintainer_agent_id,
                    Some(project_id),
                    agents::ContainerSource::FreshImage,
                )
                .await?;
            agents::ensure_agent_container_with_source(
                self.as_ref(),
                &maintainer,
                AgentStatus::Idle,
                &source,
                None,
            )
            .await?;
            Ok(())
        }
        .await;

        let update = match setup_result {
            Ok(()) => Ok(self.project(project_id).await?.summary.read().await.clone()),
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
        projects::review::worker::start_enabled_project_review_workers(Arc::clone(self)).await
    }

    async fn reconcile_project_review_singletons(self: &Arc<Self>) {
        projects::review::worker::reconcile_project_review_singletons(
            Arc::clone(self),
            PROJECT_REVIEW_RUN_LIST_LIMIT,
        )
        .await
    }

    async fn start_project_review_loop_if_ready(
        self: &Arc<Self>,
        project_id: ProjectId,
    ) -> Result<()> {
        projects::review::worker::start_project_review_loop_if_ready(Arc::clone(self), project_id)
            .await
    }

    async fn stop_project_review_loop(self: &Arc<Self>, project_id: ProjectId) {
        projects::review::worker::stop_project_review_loop(
            Arc::clone(self),
            project_id,
            PROJECT_REVIEW_RUN_LIST_LIMIT,
        )
        .await
    }

    async fn run_project_review_once(
        self: &Arc<Self>,
        project_id: ProjectId,
        cancellation_token: CancellationToken,
        target_pr: Option<u64>,
    ) -> Result<ProjectReviewCycleResult> {
        projects::review::cycle::run_project_review_once(
            self,
            project_id,
            cancellation_token,
            target_pr,
        )
        .await
    }

    async fn ensure_project_review_workspace(&self, project_id: ProjectId) -> Result<()> {
        self.sync_project_review_repo(project_id).await
    }

    async fn sync_project_review_repo(&self, project_id: ProjectId) -> Result<()> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let token = self.project_git_token(project_id).await?.ok_or_else(|| {
            RuntimeError::InvalidInput("project git account token is not configured".to_string())
        })?;
        self.workspace_manager
            .sync_repo_cache(&summary, &token)
            .await
            .map(|_| ())
    }

    async fn set_project_review_state(
        &self,
        project_id: ProjectId,
        status: ProjectReviewStatus,
        update: ReviewStateUpdate,
    ) -> Result<ProjectSummary> {
        projects::review::state::set_project_review_state(self, project_id, status, update).await
    }

    async fn delete_project_review_workspace(&self, project_id: ProjectId) -> Result<()> {
        self.workspace_manager
            .delete_project_workspace(project_id)
            .await?;
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
        projects::mcp::execute_project_mcp_tool(
            self,
            agent,
            model_name,
            arguments,
            cancellation_token,
        )
        .await
    }

    async fn agent_resource_broker(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> Result<AgentResourceBroker> {
        agents::agent_resource_broker(self, agent, agent_id, cancellation_token).await
    }

    async fn execute_project_github_api_get(
        &self,
        agent: &AgentRecord,
        path: &str,
    ) -> Result<ToolExecution> {
        github::execute_project_github_api_get(
            &self.deps.github_http,
            &self.github_api_base_url,
            self.project_git_token_for_agent(agent).await?,
            path,
        )
        .await
    }

    async fn execute_project_git_tool(
        &self,
        agent: &AgentRecord,
        name: &str,
        arguments: Value,
    ) -> Result<ToolExecution> {
        let summary = agent.summary.read().await.clone();
        let project_id = summary.project_id.ok_or_else(|| {
            RuntimeError::InvalidInput("agent is not attached to a project".to_string())
        })?;
        let project = self.project(project_id).await?;
        let project_summary = project.summary.read().await.clone();
        tools::git::execute_git_tool(
            tools::git::GitToolContext {
                git_binary: &self.git_binary,
                projects_root: &self.projects_root,
                agent_id: summary.id,
                project: project_summary,
                token: self.project_git_token(project_id).await?,
            },
            name,
            arguments,
        )
        .await
    }

    async fn clone_project_repository(
        &self,
        project_id: ProjectId,
        maintainer_agent_id: AgentId,
    ) -> Result<()> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let account_id = summary.git_account_id.clone().ok_or_else(|| {
            RuntimeError::InvalidInput("project git account is not configured".to_string())
        })?;
        let token = self.deps.git_accounts.token(&account_id).await?;
        self.workspace_manager
            .ensure_repo_cache(&summary, &token)
            .await?;
        self.workspace_manager
            .sync_repo_cache(&summary, &token)
            .await?;
        let clone_path = self
            .workspace_manager
            .prepare_agent_clone(
                &summary,
                maintainer_agent_id,
                projects::workspace::CloneSeed::DefaultBranch,
            )
            .await?
            .path;
        let existing = projects::skills::detect_existing_dirs_in_host_repo(&clone_path);
        self.refresh_project_skill_cache(project_id, &existing)
            .await?;
        Ok(())
    }

    async fn set_task_status(
        &self,
        task: &Arc<TaskRecord>,
        status: TaskStatus,
        final_report: Option<String>,
        error: Option<String>,
    ) -> Result<()> {
        tasks::set_status(&self.state, self, task, status, final_report, error).await
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
        let reasoning_effort = agents::normalize_reasoning_effort(
            &selection.model,
            preference.and_then(|item| item.reasoning_effort.as_deref()),
            true,
        )?;
        Ok(resolved_agent_model(selection, reasoning_effort))
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
        agents::ensure_agent_container(self, &agent, ready_status).await
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
        agents::ensure_agent_container(self, agent, status)
            .await
            .map(|_| ())
    }
}

impl agents::AgentCancelOps for Arc<AgentRuntime> {
    fn agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<Arc<AgentRecord>>> + Send {
        AgentRuntime::agent(self.as_ref(), agent_id)
    }

    fn set_agent_status(
        &self,
        agent: &Arc<AgentRecord>,
        status: AgentStatus,
        error: Option<String>,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::set_status(self.as_ref(), agent, status, error)
    }

    fn complete_turn_if_current(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        result: turn::completion::TurnResult,
    ) -> impl std::future::Future<Output = Result<bool>> + Send {
        turn::completion::complete_turn_if_current(
            self.deps.store.as_ref(),
            &self.events,
            agent,
            agent_id,
            result,
        )
    }

    async fn start_next_queued_input_after_turn(&self, agent_id: AgentId) {
        if let Err(err) = agents::start_next_queued_input(self.as_ref(), self, agent_id).await {
            tracing::warn!("failed to start queued agent input: {err}");
        }
    }

    fn turn_cancel_grace(&self) -> Duration {
        TURN_CANCEL_GRACE
    }
}

impl agents::AgentTurnTaskOps for Arc<AgentRuntime> {
    fn run_turn_task(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let runtime = Arc::clone(self);
        async move {
            runtime
                .run_turn(
                    agent_id,
                    session_id,
                    turn_id,
                    message,
                    skill_mentions,
                    cancellation_token,
                )
                .await;
        }
    }
}

impl agents::AgentFileOps for AgentRuntime {
    fn container_id(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<String>> + Send {
        AgentRuntime::container_id(self, agent_id)
    }

    async fn copy_to_container(
        &self,
        container_id: String,
        local_path: PathBuf,
        container_path: String,
    ) -> Result<()> {
        self.deps
            .docker
            .copy_to_container(&container_id, &local_path, &container_path)
            .await?;
        Ok(())
    }

    async fn copy_from_container_tar(
        &self,
        container_id: String,
        container_path: String,
    ) -> Result<Vec<u8>> {
        Ok(self
            .deps
            .docker
            .copy_from_container_tar(&container_id, &container_path)
            .await?)
    }
}

impl agents::AgentObservabilityOps for AgentRuntime {
    fn agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<Arc<AgentRecord>>> + Send {
        AgentRuntime::agent(self, agent_id)
    }

    async fn load_tool_trace(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        call_id: String,
    ) -> Result<Option<ToolTraceDetail>> {
        Ok(self
            .deps
            .store
            .load_tool_trace(agent_id, session_id, &call_id)
            .await?)
    }

    async fn tool_metadata(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        call_id: String,
    ) -> (Option<bool>, Option<u64>) {
        self.events
            .tool_metadata(agent_id, session_id, &call_id)
            .await
    }

    async fn list_agent_logs(
        &self,
        agent_id: AgentId,
        filter: AgentLogFilter,
    ) -> Result<Vec<AgentLogEntry>> {
        Ok(self.deps.store.list_agent_logs(agent_id, filter).await?)
    }

    async fn list_tool_traces(
        &self,
        agent_id: AgentId,
        filter: ToolTraceFilter,
    ) -> Result<Vec<ToolTraceSummary>> {
        Ok(self.deps.store.list_tool_traces(agent_id, filter).await?)
    }

    fn tool_output_artifact_file_path(
        &self,
        agent_id: AgentId,
        call_id: &str,
        artifact_id: &str,
        name: &str,
    ) -> PathBuf {
        AgentRuntime::tool_output_artifact_file_path(self, agent_id, call_id, artifact_id, name)
    }
}

impl agents::AgentResourceBrokerOps for AgentRuntime {
    fn project_mcp_manager_for_agent(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> impl std::future::Future<Output = Result<Option<Arc<McpAgentManager>>>> + Send {
        AgentRuntime::project_mcp_manager_for_agent(self, agent, agent_id, cancellation_token)
    }

    fn project_skill_read_guard(
        &self,
        agent: &AgentRecord,
    ) -> impl std::future::Future<Output = Option<tokio::sync::OwnedRwLockReadGuard<()>>> + Send
    {
        AgentRuntime::project_skill_read_guard(self, agent)
    }

    async fn skills_config(&self) -> Result<SkillsConfigRequest> {
        Ok(self.deps.store.load_skills_config().await?)
    }

    fn skills_manager_for_agent(
        &self,
        agent: &AgentRecord,
    ) -> impl std::future::Future<Output = Result<SkillsManager>> + Send {
        AgentRuntime::skills_manager_for_agent(self, agent)
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

impl agents::AgentDeleteOps for AgentRuntime {
    fn agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<Arc<AgentRecord>>> + Send {
        AgentRuntime::agent(self, agent_id)
    }

    async fn agent_summaries(&self) -> Vec<AgentSummary> {
        let agents = self.state.agents.read().await;
        let mut summaries = Vec::with_capacity(agents.len());
        for agent in agents.values() {
            summaries.push(agent.summary.read().await.clone());
        }
        summaries
    }

    async fn set_agent_status(
        &self,
        agent: Arc<AgentRecord>,
        change: agents::AgentDeleteStatusChange,
    ) -> Result<()> {
        AgentRuntime::set_status(self, &agent, change.status, change.error).await
    }

    async fn delete_agent_containers(
        &self,
        request: agents::AgentContainerDeleteRequest,
    ) -> Result<Vec<String>> {
        Ok(self
            .deps
            .docker
            .delete_agent_containers(
                &request.agent_id.to_string(),
                request.preferred_container_id.as_deref(),
            )
            .await?)
    }

    fn cleanup_project_agent_clone(
        &self,
        project_id: ProjectId,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        self.workspace_manager
            .cleanup_agent_clone(project_id, agent_id)
    }

    async fn delete_agent_from_store(&self, agent_id: AgentId) -> Result<()> {
        self.deps.store.delete_agent(agent_id).await?;
        Ok(())
    }

    async fn remove_agent_from_memory(&self, agent_id: AgentId) {
        self.state.agents.write().await.remove(&agent_id);
    }

    async fn publish_agent_deleted(&self, agent_id: AgentId) {
        self.events
            .publish(ServiceEventKind::AgentDeleted { agent_id })
            .await;
    }
}

impl agents::AgentUpdateOps for AgentRuntime {
    fn agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<Arc<AgentRecord>>> + Send {
        AgentRuntime::agent(self, agent_id)
    }

    async fn resolve_provider(
        &self,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<ProviderSelection> {
        Ok(self.deps.store.resolve_provider(provider_id, model).await?)
    }

    async fn persist_agent(&self, agent: Arc<AgentRecord>) -> Result<()> {
        AgentRuntime::persist_agent(self, &agent).await
    }

    async fn publish_agent_updated(&self, agent: AgentSummary) {
        self.events
            .publish(ServiceEventKind::AgentUpdated { agent })
            .await;
    }
}

impl agents::AgentCreateOps for AgentRuntime {
    fn default_docker_image(&self) -> String {
        self.deps.docker.image().to_string()
    }

    async fn resolve_provider(
        &self,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<ProviderSelection> {
        Ok(self.deps.store.resolve_provider(provider_id, model).await?)
    }

    async fn save_agent(&self, summary: &AgentSummary, system_prompt: Option<&str>) -> Result<()> {
        self.deps.store.save_agent(summary, system_prompt).await?;
        Ok(())
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

    async fn insert_agent(&self, agent: Arc<AgentRecord>) {
        let id = agent.summary.read().await.id;
        self.state.agents.write().await.insert(id, agent);
    }

    async fn publish_agent_created(&self, agent: AgentSummary) {
        self.events
            .publish(ServiceEventKind::AgentCreated { agent })
            .await;
    }
}

impl agents::AgentSpawnOps for Arc<AgentRuntime> {
    fn agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<Arc<AgentRecord>>> + Send {
        AgentRuntime::agent(self.as_ref(), agent_id)
    }

    fn ensure_agent_container(
        &self,
        agent: &Arc<AgentRecord>,
        ready_status: AgentStatus,
    ) -> impl std::future::Future<Output = Result<String>> + Send {
        agents::ensure_agent_container(self.as_ref(), agent, ready_status)
    }

    async fn role_model(&self, role: AgentRole) -> Result<AgentModelPreference> {
        Ok(self.resolve_role_agent_model(role).await?.preference)
    }

    fn create_agent_with_container_source(
        &self,
        request: CreateAgentRequest,
        source: agents::ContainerSource,
        task_id: Option<TaskId>,
        project_id: Option<ProjectId>,
        role: Option<AgentRole>,
    ) -> impl std::future::Future<Output = Result<AgentSummary>> + Send {
        AgentRuntime::create_agent_with_container_source(
            self, request, source, task_id, project_id, role,
        )
    }

    fn fork_agent_context(
        &self,
        parent_id: AgentId,
        child_id: AgentId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::fork_agent_context(self.as_ref(), parent_id, child_id)
    }

    fn resolve_session_id(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> impl std::future::Future<Output = Result<SessionId>> + Send {
        AgentRuntime::resolve_session_id(self.as_ref(), agent_id, session_id)
    }

    fn prepare_turn(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<(Arc<AgentRecord>, TurnId)>> + Send {
        AgentRuntime::prepare_turn(self.as_ref(), agent_id)
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

impl agents::AgentContainerOps for AgentRuntime {
    async fn start_agent_container(
        &self,
        request: agents::AgentContainerStartRequest,
    ) -> Result<ContainerHandle> {
        match request.source {
            agents::ContainerSource::FreshImage => Ok(self
                .deps
                .docker
                .ensure_agent_container_from_image(
                    &request.agent_id.to_string(),
                    request.preferred_container_id.as_deref(),
                    &request.docker_image,
                )
                .await?),
            agents::ContainerSource::ProjectClone { clone_path } => Ok(self
                .deps
                .docker
                .ensure_agent_container_from_image_with_workspace_and_repo_mount(
                    &request.agent_id.to_string(),
                    request.preferred_container_id.as_deref(),
                    &request.docker_image,
                    None,
                    Some(&clone_path),
                )
                .await?),
            agents::ContainerSource::CloneFrom {
                parent_container_id,
                docker_image,
                workspace_volume,
                repo_mount,
            } => {
                if request.preferred_container_id.is_some()
                    && workspace_volume.is_none()
                    && repo_mount.is_none()
                {
                    Ok(self
                        .deps
                        .docker
                        .ensure_agent_container_from_image(
                            &request.agent_id.to_string(),
                            request.preferred_container_id.as_deref(),
                            &docker_image,
                        )
                        .await?)
                } else {
                    Ok(self
                        .deps
                        .docker
                        .create_agent_container_from_parent_with_workspace_and_repo_mount(
                            &request.agent_id.to_string(),
                            &parent_container_id,
                            workspace_volume.as_deref(),
                            repo_mount.as_deref(),
                        )
                        .await?)
                }
            }
        }
    }

    async fn remove_agent_container(&self, agent_id: AgentId, container_id: String) {
        let _ = self
            .deps
            .docker
            .delete_agent_containers(&agent_id.to_string(), Some(&container_id))
            .await;
    }

    async fn agent_mcp_server_configs(
        &self,
    ) -> Result<std::collections::BTreeMap<String, McpServerConfig>> {
        Ok(self
            .deps
            .store
            .list_mcp_servers()
            .await?
            .into_iter()
            .filter(|(_, config)| config.scope == McpServerScope::Agent)
            .collect())
    }

    async fn start_agent_mcp_manager(
        &self,
        container_id: String,
        configs: std::collections::BTreeMap<String, McpServerConfig>,
    ) -> McpAgentManager {
        McpAgentManager::start(self.deps.docker.clone(), container_id, configs).await
    }

    async fn set_agent_status(
        &self,
        agent: Arc<AgentRecord>,
        change: agents::AgentContainerStatusChange,
    ) -> Result<()> {
        AgentRuntime::set_status(self, &agent, change.status, change.error).await
    }

    async fn persist_agent(&self, agent: Arc<AgentRecord>) -> Result<()> {
        AgentRuntime::persist_agent(self, &agent).await
    }

    async fn publish_mcp_status(&self, change: agents::AgentMcpStatusChange) {
        self.events
            .publish(ServiceEventKind::McpServerStatusChanged {
                agent_id: change.agent_id,
                server: change.server,
                status: change.status,
                error: change.error,
            })
            .await;
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

impl projects::review::state::ProjectReviewStateOps for AgentRuntime {
    fn project(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Result<Arc<ProjectRecord>>> + Send {
        AgentRuntime::project(self, project_id)
    }

    async fn save_project(&self, project: ProjectSummary) -> Result<()> {
        self.deps.store.save_project(&project).await?;
        Ok(())
    }

    async fn publish_project_updated(&self, project: ProjectSummary) {
        self.events
            .publish(ServiceEventKind::ProjectUpdated { project })
            .await;
    }
}

impl projects::review::cleanup::ProjectReviewCleanupOps for Arc<AgentRuntime> {
    async fn prune_project_review_runs_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        Ok(self
            .deps
            .store
            .prune_project_review_runs_before(cutoff)
            .await?)
    }

    async fn prune_service_events_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        Ok(self.deps.store.prune_service_events_before(cutoff).await?)
    }

    async fn prune_agent_logs_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        Ok(self.deps.store.prune_agent_logs_before(cutoff).await?)
    }

    async fn prune_tool_traces_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        Ok(self.deps.store.prune_tool_traces_before(cutoff).await?)
    }

    async fn retain_events_since(&self, cutoff: DateTime<Utc>) {
        self.events.retain_since(cutoff).await;
    }

    async fn list_projects(&self) -> Vec<ProjectSummary> {
        AgentRuntime::list_projects(self.as_ref()).await
    }
}

impl projects::mcp::ProjectMcpToolOps for AgentRuntime {
    fn project_mcp_manager_for_agent(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> impl std::future::Future<Output = Result<Option<Arc<McpAgentManager>>>> + Send {
        AgentRuntime::project_mcp_manager_for_agent(self, agent, agent_id, cancellation_token)
    }

    fn project_git_token_for_agent(
        &self,
        agent: &AgentRecord,
    ) -> impl std::future::Future<Output = Result<Option<String>>> + Send {
        AgentRuntime::project_git_token_for_agent(self, agent)
    }
}

impl projects::review::reviewer::ProjectReviewerAgentOps for Arc<AgentRuntime> {
    async fn project_summary(&self, project_id: ProjectId) -> Result<ProjectSummary> {
        let project = AgentRuntime::project(self.as_ref(), project_id).await?;
        Ok(project.summary.read().await.clone())
    }

    async fn agent_summary(&self, agent_id: AgentId) -> Result<AgentSummary> {
        let agent = AgentRuntime::agent(self.as_ref(), agent_id).await?;
        Ok(agent.summary.read().await.clone())
    }

    async fn reviewer_model(&self) -> Result<AgentModelPreference> {
        Ok(self
            .resolve_role_agent_model(AgentRole::Reviewer)
            .await?
            .preference)
    }

    fn create_agent_with_container_source(
        &self,
        request: CreateAgentRequest,
        source: agents::ContainerSource,
        task_id: Option<TaskId>,
        project_id: Option<ProjectId>,
        role: Option<AgentRole>,
    ) -> impl std::future::Future<Output = Result<AgentSummary>> + Send {
        AgentRuntime::create_agent_with_container_source(
            self, request, source, task_id, project_id, role,
        )
    }

    fn start_agent_turn(
        &self,
        agent_id: AgentId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> impl std::future::Future<Output = Result<TurnId>> + Send {
        agents::start_agent_turn(self.as_ref(), self, agent_id, message, skill_mentions)
    }

    async fn last_turn_response(&self, agent_id: AgentId) -> Result<Option<String>> {
        let agent = AgentRuntime::agent(self.as_ref(), agent_id).await?;
        let sessions = agent.sessions.lock().await;
        Ok(agents::last_turn_response(&sessions))
    }
}

impl projects::review::selector::ProjectReviewSelectorOps for Arc<AgentRuntime> {
    async fn project_summary(&self, project_id: ProjectId) -> Result<ProjectSummary> {
        let project = AgentRuntime::project(self.as_ref(), project_id).await?;
        Ok(project.summary.read().await.clone())
    }

    async fn agent_summary(&self, agent_id: AgentId) -> Result<AgentSummary> {
        let agent = AgentRuntime::agent(self.as_ref(), agent_id).await?;
        Ok(agent.summary.read().await.clone())
    }

    async fn selector_model(&self) -> Result<AgentModelPreference> {
        Ok(self
            .resolve_role_agent_model(AgentRole::Explorer)
            .await?
            .preference)
    }

    fn create_agent_with_container_source(
        &self,
        request: CreateAgentRequest,
        source: agents::ContainerSource,
        task_id: Option<TaskId>,
        project_id: Option<ProjectId>,
        role: Option<AgentRole>,
    ) -> impl std::future::Future<Output = Result<AgentSummary>> + Send {
        AgentRuntime::create_agent_with_container_source(
            self, request, source, task_id, project_id, role,
        )
    }

    fn start_agent_turn(
        &self,
        agent_id: AgentId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> impl std::future::Future<Output = Result<TurnId>> + Send {
        agents::start_agent_turn(self.as_ref(), self, agent_id, message, skill_mentions)
    }

    fn wait_agent_until_complete_with_cancel(
        &self,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> impl std::future::Future<Output = Result<AgentSummary>> + Send {
        AgentRuntime::wait_agent_until_complete_with_cancel(
            self.as_ref(),
            agent_id,
            cancellation_token,
        )
    }

    async fn last_turn_response(&self, agent_id: AgentId) -> Result<Option<String>> {
        let agent = AgentRuntime::agent(self.as_ref(), agent_id).await?;
        let sessions = agent.sessions.lock().await;
        Ok(agents::last_turn_response(&sessions))
    }

    fn delete_agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::delete_agent(self.as_ref(), agent_id)
    }
}

impl projects::review::cycle::ProjectReviewCycleOps for Arc<AgentRuntime> {
    async fn set_project_review_state(
        &self,
        project_id: ProjectId,
        status: ProjectReviewStatus,
        update: ReviewStateUpdate,
    ) -> Result<ProjectSummary> {
        AgentRuntime::set_project_review_state(self.as_ref(), project_id, status, update).await
    }

    async fn save_project_review_run_status(&self, summary: ProjectReviewRunSummary) -> Result<()> {
        projects::review::runs::save_project_review_run_status(
            &self.deps.store,
            summary,
            Vec::new(),
            Vec::new(),
        )
        .await
    }

    async fn load_project_review_run(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
    ) -> Result<Option<ProjectReviewRunDetail>> {
        Ok(self
            .deps
            .store
            .load_project_review_run(project_id, run_id)
            .await?)
    }

    async fn update_project_review_run_turn(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
        reviewer_agent_id: AgentId,
        turn_id: TurnId,
    ) -> Result<()> {
        projects::review::runs::update_project_review_run_turn(
            &self.deps.store,
            project_id,
            run_id,
            reviewer_agent_id,
            turn_id,
        )
        .await
    }

    async fn finish_project_review_run(&self, request: FinishReviewRun) -> Result<()> {
        projects::review::runs::finish_project_review_run(&self.deps.store, self.as_ref(), request)
            .await
    }

    fn sync_project_review_repo(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::sync_project_review_repo(self.as_ref(), project_id)
    }

    fn refresh_project_skills_from_review_workspace(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::refresh_project_skills_from_review_workspace(self.as_ref(), project_id)
    }

    fn spawn_project_reviewer_agent(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Result<AgentSummary>> + Send {
        projects::review::reviewer::spawn_project_reviewer_agent(self, project_id)
    }

    fn project_reviewer_initial_message(
        &self,
        project_id: ProjectId,
        reviewer_id: AgentId,
        target_pr: Option<u64>,
    ) -> impl std::future::Future<Output = Result<String>> + Send {
        projects::review::reviewer::project_reviewer_initial_message(
            self,
            project_id,
            reviewer_id,
            target_pr,
        )
    }

    fn start_reviewer_turn(
        &self,
        reviewer_id: AgentId,
        message: String,
    ) -> impl std::future::Future<Output = Result<TurnId>> + Send {
        projects::review::reviewer::start_reviewer_turn(self, reviewer_id, message)
    }

    fn wait_agent_until_complete_with_cancel(
        &self,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> impl std::future::Future<Output = Result<AgentSummary>> + Send {
        AgentRuntime::wait_agent_until_complete_with_cancel(
            self.as_ref(),
            agent_id,
            cancellation_token,
        )
    }

    fn reviewer_final_response(
        &self,
        reviewer_id: AgentId,
    ) -> impl std::future::Future<Output = Result<String>> + Send {
        projects::review::reviewer::last_turn_response(self, reviewer_id)
    }

    fn delete_agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::delete_agent(self.as_ref(), agent_id)
    }
}

impl projects::review::worker::ProjectReviewWorkerOps for Arc<AgentRuntime> {
    fn project(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Result<Arc<ProjectRecord>>> + Send {
        AgentRuntime::project(self.as_ref(), project_id)
    }

    async fn project_ids(&self) -> Vec<ProjectId> {
        let projects = self.state.projects.read().await;
        projects.keys().copied().collect()
    }

    fn project_auto_reviewer_agents(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Vec<AgentSummary>> + Send {
        AgentRuntime::project_auto_reviewer_agents(self.as_ref(), project_id)
    }

    async fn load_project_review_runs(
        &self,
        project_id: ProjectId,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<ProjectReviewRunSummary>> {
        Ok(self
            .deps
            .store
            .load_project_review_runs(project_id, None, offset, limit)
            .await?)
    }

    async fn finish_project_review_run(&self, request: FinishReviewRun) -> Result<()> {
        projects::review::runs::finish_project_review_run(&self.deps.store, self.as_ref(), request)
            .await
    }

    async fn cancel_active_project_review_runs(
        &self,
        project_id: ProjectId,
        reviewer_agent_id: Option<AgentId>,
        run_list_limit: usize,
    ) -> Result<()> {
        projects::review::runs::cancel_active_project_review_runs(
            &self.deps.store,
            self.as_ref(),
            project_id,
            reviewer_agent_id,
            run_list_limit,
        )
        .await
    }

    async fn record_project_review_startup_failure(
        &self,
        project_id: ProjectId,
        error: String,
    ) -> Result<()> {
        projects::review::runs::record_project_review_startup_failure(
            &self.deps.store,
            project_id,
            error,
        )
        .await
    }

    async fn set_project_review_state(
        &self,
        project_id: ProjectId,
        status: ProjectReviewStatus,
        update: ReviewStateUpdate,
    ) -> Result<ProjectSummary> {
        AgentRuntime::set_project_review_state(self.as_ref(), project_id, status, update).await
    }

    fn ensure_project_review_workspace(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::ensure_project_review_workspace(self.as_ref(), project_id)
    }

    async fn project_git_provider(&self, project_id: ProjectId) -> Result<Option<GitProvider>> {
        let project = AgentRuntime::project(self.as_ref(), project_id).await?;
        let Some(account_id) = project.summary.read().await.git_account_id.clone() else {
            return Ok(None);
        };
        Ok(Some(
            self.deps.git_accounts.summary(&account_id).await?.provider,
        ))
    }

    fn run_project_review_selector(
        &self,
        project_id: ProjectId,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        projects::review::selector::run_project_review_selector(
            self,
            project_id,
            cancellation_token,
        )
    }

    fn run_project_review_once(
        &self,
        project_id: ProjectId,
        cancellation_token: CancellationToken,
        target_pr: Option<u64>,
    ) -> impl std::future::Future<Output = Result<ProjectReviewCycleResult>> + Send {
        AgentRuntime::run_project_review_once(self, project_id, cancellation_token, target_pr)
    }

    async fn agent_current_turn(&self, agent_id: AgentId) -> Result<Option<TurnId>> {
        let agent = AgentRuntime::agent(self.as_ref(), agent_id).await?;
        Ok(agent.summary.read().await.current_turn)
    }

    fn cancel_agent_turn(
        &self,
        agent_id: AgentId,
        turn_id: TurnId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::cancel_agent_turn(self, agent_id, turn_id)
    }

    fn delete_agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::delete_agent(self.as_ref(), agent_id)
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

impl tasks::TaskReadOps for AgentRuntime {
    fn get_agent(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> impl std::future::Future<Output = Result<AgentDetail>> + Send {
        AgentRuntime::get_agent(self, agent_id, session_id)
    }
}

impl tasks::TaskUpdateOps for AgentRuntime {
    async fn save_task(&self, summary: &TaskSummary, plan: &TaskPlan) -> Result<()> {
        self.deps.store.save_task(summary, plan).await?;
        Ok(())
    }

    async fn publish_task_updated(&self, task: TaskSummary) {
        self.events
            .publish(ServiceEventKind::TaskUpdated { task })
            .await;
    }
}

impl tasks::TaskPlanOps for AgentRuntime {
    async fn save_plan_history_entry(
        &self,
        task_id: TaskId,
        entry: &PlanHistoryEntry,
    ) -> Result<()> {
        self.deps
            .store
            .save_plan_history_entry(task_id, entry)
            .await?;
        Ok(())
    }

    async fn publish_plan_updated(&self, task_id: TaskId, plan: TaskPlan) {
        self.events
            .publish(ServiceEventKind::PlanUpdated { task_id, plan })
            .await;
    }
}

impl tasks::TaskToolOps for AgentRuntime {
    async fn agent_summary(&self, agent_id: AgentId) -> Result<AgentSummary> {
        let agent = self.agent(agent_id).await?;
        Ok(agent.summary.read().await.clone())
    }

    async fn append_task_review(&self, review: &TaskReview) -> Result<()> {
        self.deps.store.append_task_review(review).await?;
        Ok(())
    }
}

impl tasks::TaskPlanningOps for Arc<AgentRuntime> {
    async fn send_agent_message(
        &self,
        agent_id: AgentId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        AgentRuntime::send_message(self, agent_id, None, message, skill_mentions).await
    }

    async fn spawn_task_workflow(&self, task_id: TaskId) {
        AgentRuntime::spawn_task_workflow(self, task_id);
    }
}

impl tasks::TaskLifecycleOps for Arc<AgentRuntime> {
    async fn cancel_agent_for_task(
        &self,
        agent_id: AgentId,
        current_turn: Option<TurnId>,
    ) -> Result<()> {
        if let Some(turn_id) = current_turn {
            AgentRuntime::cancel_agent_turn(self, agent_id, turn_id).await
        } else {
            let record = self.agent(agent_id).await?;
            record.cancel_requested.store(true, Ordering::SeqCst);
            self.set_status(&record, AgentStatus::Cancelled, None).await
        }
    }

    async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
        AgentRuntime::delete_agent(self, agent_id).await
    }

    async fn agent_current_turn(&self, agent_id: AgentId) -> Result<Option<TurnId>> {
        let record = self.agent(agent_id).await?;
        Ok(record.summary.read().await.current_turn)
    }

    async fn delete_task_from_store(&self, task_id: TaskId) -> Result<()> {
        self.deps.store.delete_task(task_id).await?;
        Ok(())
    }

    async fn publish_task_deleted(&self, task_id: TaskId) {
        self.events
            .publish(ServiceEventKind::TaskDeleted { task_id })
            .await;
    }
}

impl tasks::TaskWorkflowOps for Arc<AgentRuntime> {
    async fn spawn_task_role_agent(
        &self,
        parent_agent_id: AgentId,
        role: AgentRole,
        name: Option<String>,
    ) -> Result<AgentSummary> {
        AgentRuntime::spawn_task_role_agent(self, parent_agent_id, role, name).await
    }

    async fn start_agent_turn(&self, agent_id: AgentId, message: String) -> Result<TurnId> {
        AgentRuntime::start_agent_turn(self, agent_id, message).await
    }

    async fn wait_agent(&self, agent_id: AgentId, timeout: Duration) -> Result<AgentSummary> {
        AgentRuntime::wait_agent(self.as_ref(), agent_id, timeout).await
    }
}

impl tasks::TaskArtifactOps for AgentRuntime {
    async fn agent_task_id(&self, agent_id: AgentId) -> Result<Option<TaskId>> {
        let agent = self.agent(agent_id).await?;
        Ok(agent.summary.read().await.task_id)
    }

    async fn agent_container_id(&self, agent_id: AgentId) -> Result<String> {
        self.container_id(agent_id).await
    }

    fn artifact_files_root(&self) -> PathBuf {
        self.artifact_files_root.clone()
    }

    async fn copy_artifact_from_container(
        &self,
        container_id: String,
        source_path: String,
        dest_path: PathBuf,
    ) -> Result<()> {
        self.deps
            .docker
            .copy_from_container_to_file(&container_id, &source_path, &dest_path)
            .await?;
        Ok(())
    }

    fn save_artifact_record(&self, info: &ArtifactInfo) -> Result<()> {
        self.deps.store.save_artifact(info)?;
        Ok(())
    }

    async fn publish_artifact_created(&self, info: ArtifactInfo) {
        self.events
            .publish(ServiceEventKind::ArtifactCreated { artifact: info })
            .await;
    }
}

impl tasks::TaskCreateOps for Arc<AgentRuntime> {
    async fn planner_model(&self) -> Result<AgentModelPreference> {
        Ok(self
            .resolve_role_agent_model(AgentRole::Planner)
            .await?
            .preference)
    }

    async fn create_task_planner_agent(
        &self,
        request: tasks::CreateTaskPlannerAgentRequest,
    ) -> Result<AgentSummary> {
        self.create_agent_with_container_source(
            CreateAgentRequest {
                name: Some(format!("{} Planner", request.title)),
                provider_id: Some(request.model.provider_id),
                model: Some(request.model.model),
                reasoning_effort: request.model.reasoning_effort,
                docker_image: request.docker_image,
                parent_id: None,
                system_prompt: Some(
                    agents::task_role_system_prompt(AgentRole::Planner).to_string(),
                ),
            },
            agents::ContainerSource::FreshImage,
            Some(request.task_id),
            None,
            Some(AgentRole::Planner),
        )
        .await
    }

    async fn save_task(&self, summary: &TaskSummary, plan: &TaskPlan) -> Result<()> {
        self.deps.store.save_task(summary, plan).await?;
        Ok(())
    }

    async fn publish_task_event(&self, event: ServiceEventKind) {
        self.events.publish(event).await;
    }

    fn send_task_message(
        &self,
        task_id: TaskId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> impl std::future::Future<Output = Result<TurnId>> + Send {
        AgentRuntime::send_task_message(self, task_id, message, skill_mentions)
    }

    async fn spawn_task_title_generation(&self, task_id: TaskId, message: String) {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            match runtime.generate_task_title(&message).await {
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

impl tasks::EnvironmentOps for Arc<AgentRuntime> {
    async fn planner_model(&self) -> Result<AgentModelPreference> {
        Ok(self
            .resolve_role_agent_model(AgentRole::Planner)
            .await?
            .preference)
    }

    async fn create_environment_root_agent(
        &self,
        request: tasks::CreateTaskPlannerAgentRequest,
    ) -> Result<AgentSummary> {
        let agent = agents::create_agent_record(
            self.as_ref(),
            CreateAgentRequest {
                name: Some(request.title),
                provider_id: Some(request.model.provider_id),
                model: Some(request.model.model),
                reasoning_effort: request.model.reasoning_effort,
                docker_image: request.docker_image,
                parent_id: None,
                system_prompt: None,
            },
            agents::CreateAgentRecordContext {
                task_id: Some(request.task_id),
                project_id: None,
                role: Some(AgentRole::Planner),
            },
        )
        .await?;
        match agents::ensure_agent_container_with_source(
            self.as_ref(),
            &agent,
            AgentStatus::Idle,
            &agents::ContainerSource::FreshImage,
            None,
        )
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
                    tracing::warn!("failed to persist environment root agent failure: {store_err}");
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

    fn get_agent(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> impl std::future::Future<Output = Result<AgentDetail>> + Send {
        AgentRuntime::get_agent(self.as_ref(), agent_id, session_id)
    }

    fn create_agent_session(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<AgentSessionSummary>> + Send {
        AgentRuntime::create_session(self.as_ref(), agent_id)
    }

    fn send_agent_message(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> impl std::future::Future<Output = Result<TurnId>> + Send {
        AgentRuntime::send_message(self, agent_id, Some(session_id), message, skill_mentions)
    }
}

impl projects::service::ProjectLifecycleOps for Arc<AgentRuntime> {
    async fn save_project(&self, project: &ProjectSummary) -> Result<()> {
        self.deps.store.save_project(project).await?;
        Ok(())
    }

    async fn delete_project_from_store(&self, project_id: ProjectId) -> Result<()> {
        self.deps.store.delete_project(project_id).await?;
        Ok(())
    }

    async fn publish_project_event(&self, event: ServiceEventKind) {
        self.events.publish(event).await;
    }

    fn start_project_review_loop_if_ready(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::start_project_review_loop_if_ready(self, project_id)
    }

    fn stop_project_review_loop(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = ()> + Send {
        AgentRuntime::stop_project_review_loop(self, project_id)
    }

    fn delete_agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::delete_agent(self.as_ref(), agent_id)
    }

    async fn cancel_project_agent(&self, agent_id: AgentId) -> Result<()> {
        if let Ok(record) = self.agent(agent_id).await {
            let current_turn = record.summary.read().await.current_turn;
            if let Some(turn_id) = current_turn {
                let _ = self.cancel_agent_turn(agent_id, turn_id).await;
            } else {
                record.cancel_requested.store(true, Ordering::SeqCst);
                let _ = self.set_status(&record, AgentStatus::Cancelled, None).await;
            }
        }
        Ok(())
    }

    fn shutdown_project_mcp_manager(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = ()> + Send {
        AgentRuntime::shutdown_project_mcp_manager(self, project_id)
    }

    async fn delete_project_sidecar(&self, project_id: ProjectId) -> Result<()> {
        AgentRuntime::delete_project_sidecar(self.as_ref(), project_id)
            .await
            .map(|_| ())
    }

    fn delete_project_review_workspace(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::delete_project_review_workspace(self.as_ref(), project_id)
    }

    async fn remove_project_from_memory(&self, project_id: ProjectId) {
        self.state.projects.write().await.remove(&project_id);
    }

    async fn remove_project_skill_lock(&self, project_id: ProjectId) {
        self.state
            .project_skill_locks
            .write()
            .await
            .remove(&project_id);
    }

    fn project_skill_cache_dir(&self, project_id: ProjectId) -> PathBuf {
        AgentRuntime::project_skill_cache_dir(self.as_ref(), project_id)
    }

    async fn send_project_agent_message(
        &self,
        agent_id: AgentId,
        request: SendMessageRequest,
    ) -> Result<TurnId> {
        AgentRuntime::send_message(
            self,
            agent_id,
            request.session_id,
            request.message,
            request.skill_mentions,
        )
        .await
    }
}

impl projects::service::ProjectCreateOps for Arc<AgentRuntime> {
    async fn list_github_installations(&self) -> Result<GithubInstallationsResponse> {
        AgentRuntime::list_github_installations(self.as_ref()).await
    }

    async fn upsert_github_app_relay_account(
        &self,
        installation_id: u64,
        account_login: &str,
    ) -> Result<String> {
        Ok(self
            .deps
            .store
            .upsert_github_app_relay_account(installation_id, account_login, "default", false)
            .await?
            .id)
    }

    async fn verified_repository(
        &self,
        account_id: &str,
        repository_full_name: &str,
    ) -> Result<github::VerifiedGithubRepository> {
        self.deps
            .git_accounts
            .verified_repository(account_id, repository_full_name)
            .await
    }

    async fn git_account_summary(&self, account_id: &str) -> Result<GitAccountSummary> {
        self.deps.git_accounts.summary(account_id).await
    }

    async fn planner_model(&self) -> Result<AgentModelPreference> {
        Ok(self
            .resolve_role_agent_model(AgentRole::Planner)
            .await?
            .preference)
    }

    async fn create_project_maintainer_agent(
        &self,
        request: projects::service::ProjectMaintainerAgentRequest,
    ) -> Result<AgentSummary> {
        let maintainer = agents::create_agent_record(
            self.as_ref(),
            CreateAgentRequest {
                name: Some(request.name),
                provider_id: Some(request.model.provider_id),
                model: Some(request.model.model),
                reasoning_effort: request.model.reasoning_effort,
                docker_image: request.docker_image,
                parent_id: None,
                system_prompt: Some(request.system_prompt),
            },
            agents::CreateAgentRecordContext {
                task_id: None,
                project_id: Some(request.project_id),
                role: Some(AgentRole::Planner),
            },
        )
        .await?;
        Ok(maintainer.summary.read().await.clone())
    }

    async fn save_project(&self, project: &ProjectSummary) -> Result<()> {
        self.deps.store.save_project(project).await?;
        Ok(())
    }

    async fn insert_project(&self, project: ProjectSummary) {
        self.state
            .projects
            .write()
            .await
            .insert(project.id, Arc::new(ProjectRecord::new(project)));
    }

    async fn publish_project_event(&self, event: ServiceEventKind) {
        self.events.publish(event).await;
    }

    async fn start_project_workspace(&self, project_id: ProjectId, maintainer_agent_id: AgentId) {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = runtime
                .start_project_workspace(project_id, maintainer_agent_id)
                .await
            {
                tracing::warn!(project_id = %project_id, "failed to finish project workspace setup: {err}");
            }
        });
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
        agents::ensure_agent_container_for_turn(
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
        let result = agents::spawn_child_agent(
            self,
            parent_agent_id,
            agents::SpawnChildAgentRequest {
                name: request.name,
                role: request.role,
                model: request.model,
                reasoning_effort: request.reasoning_effort,
                use_role_model: request.legacy_role.is_some(),
                fork_context: request.fork_context,
                collab_input: request.collab_input,
            },
        )
        .await?;
        Ok(turn::tools::SpawnAgentToolResult {
            agent: result.agent,
            turn_id: result.turn_id,
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

    async fn queue_project_review_prs(
        &self,
        agent: &AgentRecord,
        prs: Vec<turn::tools::QueueProjectReviewPr>,
    ) -> Result<ToolExecution> {
        let agent_summary = agent.summary.read().await.clone();
        let project_id = agent_summary.project_id.ok_or_else(|| {
            RuntimeError::InvalidInput(
                "queue_project_review_prs is only available to project agents".to_string(),
            )
        })?;
        if !matches!(
            agent_summary.role,
            Some(AgentRole::Explorer | AgentRole::Reviewer)
        ) {
            return Err(RuntimeError::InvalidInput(
                "queue_project_review_prs is only available to project selector and reviewer agents"
                    .to_string(),
            ));
        }

        let mut queued = Vec::new();
        let mut deduped = Vec::new();
        let mut ignored = Vec::new();
        for pr in prs {
            let reason = pr
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("selector")
                .to_string();
            let summary = self
                .enqueue_project_review(ProjectReviewQueueRequest {
                    project_id,
                    pr: pr.number,
                    head_sha: pr.head_sha,
                    delivery_id: None,
                    reason,
                })
                .await?;
            queued.extend(summary.queued);
            deduped.extend(summary.deduped);
            ignored.extend(summary.ignored);
        }
        Ok(ToolExecution::new(
            true,
            json!({
                "queued": queued,
                "deduped": deduped,
                "ignored": ignored,
            })
            .to_string(),
            false,
        ))
    }

    async fn execute_project_git_tool(
        &self,
        agent: &AgentRecord,
        name: String,
        arguments: Value,
    ) -> Result<ToolExecution> {
        AgentRuntime::execute_project_git_tool(self.as_ref(), agent, &name, arguments).await
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

#[cfg(test)]
fn extract_skill_mentions(text: &str) -> Vec<String> {
    mai_skills::extract_skill_mentions(text)
}

fn should_auto_compact(last_context_tokens: u64, context_tokens: u64) -> bool {
    if last_context_tokens == 0 || context_tokens == 0 {
        return false;
    }
    last_context_tokens.saturating_mul(100)
        >= context_tokens.saturating_mul(AUTO_COMPACT_THRESHOLD_PERCENT)
}

#[cfg(test)]
mod tests;
