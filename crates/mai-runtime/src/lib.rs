use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, TimeDelta, Utc};
use futures::future::{AbortHandle, Abortable};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use mai_docker::{ContainerCreateOptions, ContainerHandle, DockerClient};
use mai_mcp::{McpAgentManager, McpTool};
use mai_model::{ModelRequest, ModelTurnState, ResponsesClient};
use mai_protocol::{
    AgentConfigRequest, AgentConfigResponse, AgentDetail, AgentId, AgentMessage,
    AgentModelPreference, AgentRole, AgentSessionSummary, AgentStatus, AgentSummary, ArtifactInfo,
    ContextUsage, CreateAgentRequest, CreateProjectRequest, GitAccountRequest, GitAccountResponse,
    GitAccountStatus, GitAccountSummary, GitAccountsResponse, GitTokenKind,
    GithubAppManifestAccountType, GithubAppManifestStartRequest, GithubAppManifestStartResponse,
    GithubAppSettingsRequest, GithubAppSettingsResponse, GithubInstallationSummary,
    GithubInstallationsResponse, GithubRepositoriesResponse, GithubRepositorySummary,
    McpServerConfig, McpServerScope, McpServerTransport, MessageRole, ModelConfig,
    ModelContentItem, ModelInputItem, ModelOutputItem, ModelToolCall, PlanHistoryEntry, PlanStatus,
    ProjectCloneStatus, ProjectDetail, ProjectId, ProjectReviewOutcome, ProjectReviewRunDetail,
    ProjectReviewRunStatus, ProjectReviewRunSummary, ProjectReviewRunsResponse,
    ProjectReviewStatus, ProjectStatus, ProjectSummary, RepositoryPackageSummary,
    RepositoryPackagesResponse, ResolvedAgentModelPreference, RuntimeDefaultsResponse,
    SendMessageRequest, ServiceEvent, ServiceEventKind, SessionId, SkillActivationInfo, SkillScope,
    SkillsConfigRequest, SkillsListResponse, TaskDetail, TaskId, TaskPlan, TaskReview, TaskStatus,
    TaskSummary, TodoItem, TokenUsage, ToolTraceDetail, TurnId, TurnStatus, UpdateAgentRequest,
    UpdateProjectRequest, UserInputOption, UserInputQuestion, now, preview,
};
use mai_skills::{SkillInjections, SkillsManager};
use mai_store::{ConfigStore, ProviderSelection};
use mai_tools::{RoutedTool, build_tool_definitions_with_filter, route_tool};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio::time::{Duration, Instant, sleep};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const RECENT_EVENT_LIMIT: usize = 500;
const AUTO_COMPACT_THRESHOLD_PERCENT: u64 = 80;
const REVIEW_ROUND_LIMIT: u64 = 5;
const PROJECT_WORKSPACE_PATH: &str = "/workspace/repo";
const PROJECT_SKILLS_CACHE_DIR: &str = "project-skills";
const PROJECT_SKILL_CANDIDATE_DIRS: [(&str, &str); 3] = [
    (".claude/skills", "claude"),
    (".agents/skills", "agents"),
    ("skills", "skills"),
];
const DEFAULT_GITHUB_API_BASE_URL: &str = "https://api.github.com";
const DEFAULT_GITHUB_WEB_BASE_URL: &str = "https://github.com";
const GITHUB_API_VERSION: &str = "2022-11-28";
const GITHUB_TOKEN_REFRESH_SKEW_SECS: i64 = 120;
const GITHUB_MANIFEST_STATE_TTL_SECS: u64 = 900;
const GITHUB_HTTP_TIMEOUT_SECS: u64 = 10;
const PROJECT_REVIEW_IDLE_RETRY_SECS: u64 = 120;
const PROJECT_REVIEW_FAILURE_RETRY_SECS: u64 = 600;
const PROJECT_REVIEW_HISTORY_RETENTION_DAYS: i64 = 5;
const PROJECT_REVIEW_CLEANUP_INTERVAL_SECS: u64 = 3600;
const PROJECT_REVIEW_RUN_LIST_LIMIT: usize = 50;
const PROJECT_REVIEW_SNAPSHOT_MESSAGE_LIMIT: usize = 40;
const PROJECT_REVIEW_SNAPSHOT_EVENT_LIMIT: usize = 80;
const PROJECT_REVIEW_REPO_COMMAND_MAX_ATTEMPTS: usize = 3;
const PROJECT_REVIEW_REPO_COMMAND_RETRY_SECS: u64 = 5;
const PROJECT_REVIEW_GIT_LOW_SPEED_LIMIT: u64 = 1;
const PROJECT_REVIEW_GIT_LOW_SPEED_TIME_SECS: u64 = 30;
const DEFAULT_WAIT_AGENT_OBSERVATION_SECS: u64 = 30;
const PROJECT_GITHUB_MCP_SERVER: &str = "github";
const PROJECT_GIT_MCP_SERVER: &str = "git";
const PROJECT_GITHUB_PULL_REQUEST_REVIEW_WRITE_TOOL: &str =
    "mcp__github__pull_request_review_write";
const PROJECT_GITHUB_CREATE_PULL_REQUEST_REVIEW_TOOL: &str =
    "mcp__github__create_pull_request_review";
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
}

pub struct AgentRuntime {
    docker: DockerClient,
    model: ResponsesClient,
    store: Arc<ConfigStore>,
    skills: SkillsManager,
    agents: RwLock<HashMap<AgentId, Arc<AgentRecord>>>,
    tasks: RwLock<HashMap<TaskId, Arc<TaskRecord>>>,
    projects: RwLock<HashMap<ProjectId, Arc<ProjectRecord>>>,
    project_mcp_managers: RwLock<HashMap<ProjectId, Arc<McpAgentManager>>>,
    github_tokens: Mutex<HashMap<String, CachedGithubToken>>,
    github_manifest_states: Mutex<HashMap<String, GithubManifestState>>,
    github_http: reqwest::Client,
    event_tx: broadcast::Sender<ServiceEvent>,
    sequence: AtomicU64,
    recent_events: Mutex<VecDeque<ServiceEvent>>,
    cache_root: PathBuf,
    artifact_files_root: PathBuf,
    sidecar_image: String,
    github_api_base_url: String,
}

struct ProjectRecord {
    summary: RwLock<ProjectSummary>,
    sidecar: RwLock<Option<ContainerHandle>>,
    review_worker: Mutex<Option<ProjectReviewWorker>>,
}

struct ProjectReviewWorker {
    cancellation_token: CancellationToken,
    abort_handle: AbortHandle,
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
    active_turn: StdMutex<Option<TurnControl>>,
    pending_inputs: Mutex<VecDeque<QueuedAgentInput>>,
}

#[derive(Clone)]
struct TurnControl {
    turn_id: TurnId,
    session_id: SessionId,
    cancellation_token: CancellationToken,
    abort_handle: Option<AbortHandle>,
}

#[derive(Clone)]
struct TurnGuard {
    turn_id: TurnId,
    cancellation_token: CancellationToken,
}

#[derive(Clone)]
struct AgentSessionRecord {
    summary: AgentSessionSummary,
    messages: Vec<AgentMessage>,
    history: Vec<ModelInputItem>,
    last_context_tokens: Option<u64>,
    last_turn_response: Option<String>,
}

#[derive(Clone, Debug)]
struct QueuedAgentInput {
    session_id: Option<SessionId>,
    message: String,
    skill_mentions: Vec<String>,
}

#[derive(Debug, Default)]
struct CollabInput {
    message: Option<String>,
    skill_mentions: Vec<String>,
}

#[derive(Debug)]
struct ToolExecution {
    success: bool,
    output: String,
    ends_turn: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ProjectReviewCycleReport {
    outcome: ProjectReviewOutcome,
    #[serde(default)]
    pr: Option<u64>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct ProjectReviewCycleResult {
    outcome: ProjectReviewOutcome,
    pr: Option<u64>,
    summary: Option<String>,
    error: Option<String>,
}

struct ProjectReviewLoopDecision {
    delay: Duration,
    status: ProjectReviewStatus,
    outcome: Option<ProjectReviewOutcome>,
    summary: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct AgentCapability {
    can_spawn_agents: bool,
    can_close_agents: bool,
    communication: AgentCommunicationPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentCommunicationPolicy {
    All,
    ParentAndMaintainer,
}

#[derive(Debug, Clone)]
enum ContainerSource {
    FreshImage,
    CloneFrom {
        parent_container_id: String,
        docker_image: String,
        workspace_volume: Option<String>,
    },
}

#[derive(Debug, Clone, Copy)]
enum ReviewRepoCommand {
    Ensure,
    Sync,
}

#[derive(Debug, Clone)]
struct CachedGithubToken {
    token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct GithubManifestState {
    created_at: Instant,
    account_type: GithubAppManifestAccountType,
    org: Option<String>,
}

struct ResolvedAgentModel {
    preference: AgentModelPreference,
    effective: ResolvedAgentModelPreference,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectSkillSourceDir {
    relative: PathBuf,
    cache_name: String,
    container_path: String,
}

struct TurnResult {
    turn_id: TurnId,
    status: TurnStatus,
    agent_status: AgentStatus,
    final_text: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct GithubJwtClaims {
    iat: usize,
    exp: usize,
    iss: String,
}

#[derive(Debug, Deserialize)]
struct GithubAccountApi {
    login: String,
    #[serde(rename = "type")]
    account_type: String,
}

#[derive(Debug, Deserialize)]
struct GithubUserApi {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GithubInstallationApi {
    id: u64,
    account: GithubAccountApi,
    repository_selection: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubRepositoriesApi {
    repositories: Vec<GithubRepositoryApi>,
}

#[derive(Debug, Deserialize)]
struct GithubRepositoryApi {
    id: u64,
    name: String,
    full_name: String,
    private: bool,
    clone_url: String,
    html_url: String,
    default_branch: Option<String>,
    owner: GithubAccountApi,
}

#[derive(Debug, Deserialize)]
struct GithubPackageApi {
    name: String,
    html_url: String,
    #[serde(default)]
    repository: Option<GithubPackageRepositoryApi>,
}

#[derive(Debug, Deserialize)]
struct GithubPackageRepositoryApi {
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct GithubPackageVersionApi {
    #[serde(default)]
    metadata: GithubPackageVersionMetadataApi,
}

#[derive(Debug, Deserialize, Default)]
struct GithubPackageVersionMetadataApi {
    #[serde(default)]
    container: GithubPackageContainerMetadataApi,
}

#[derive(Debug, Deserialize, Default)]
struct GithubPackageContainerMetadataApi {
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Clone)]
struct VerifiedGithubRepository {
    id: u64,
    owner: String,
    name: String,
    full_name: String,
    default_branch: String,
}

#[derive(Debug, Serialize)]
struct GithubAccessTokenRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_ids: Option<Vec<u64>>,
    permissions: GithubAccessTokenPermissions,
}

#[derive(Debug, Serialize)]
struct GithubAccessTokenPermissions {
    contents: &'static str,
    pull_requests: &'static str,
    issues: &'static str,
}

#[derive(Debug, Deserialize)]
struct GithubAccessTokenResponse {
    token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct GithubManifestConversionResponse {
    id: u64,
    slug: String,
    html_url: String,
    pem: String,
    #[serde(default)]
    owner: Option<GithubAccountApi>,
}

#[derive(Debug, Deserialize)]
struct GithubErrorResponse {
    message: Option<String>,
}

impl AgentRuntime {
    pub async fn new(
        docker: DockerClient,
        model: ResponsesClient,
        store: Arc<ConfigStore>,
        config: RuntimeConfig,
    ) -> Result<Arc<Self>> {
        let skills = SkillsManager::new_with_system_root(
            &config.repo_root,
            config.system_skills_root.as_ref(),
        );
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

        let runtime = Arc::new(Self {
            docker,
            model,
            store,
            skills,
            agents: RwLock::new(agents),
            tasks: RwLock::new(tasks),
            projects: RwLock::new(projects),
            project_mcp_managers: RwLock::new(HashMap::new()),
            github_tokens: Mutex::new(HashMap::new()),
            github_manifest_states: Mutex::new(HashMap::new()),
            github_http,
            event_tx,
            sequence: AtomicU64::new(snapshot.next_sequence),
            recent_events: Mutex::new(snapshot.recent_events.into_iter().collect()),
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

    pub async fn list_skills(&self) -> Result<SkillsListResponse> {
        let config = self.store.load_skills_config().await?;
        Ok(self.skills.list(&config)?)
    }

    pub async fn update_skills_config(
        &self,
        request: SkillsConfigRequest,
    ) -> Result<SkillsListResponse> {
        let normalized = mai_skills::normalize_config(&request)?;
        self.store.save_skills_config(&normalized).await?;
        Ok(self.skills.list(&normalized)?)
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
        self.refresh_project_skill_cache(project_id, &sidecar.id, &existing)
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

    pub async fn list_projects(&self) -> Vec<ProjectSummary> {
        let project_records = {
            let projects = self.projects.read().await;
            projects.values().cloned().collect::<Vec<_>>()
        };
        let mut summaries = Vec::with_capacity(project_records.len());
        for project in project_records {
            summaries.push(project.summary.read().await.clone());
        }
        summaries.sort_by_key(|summary| summary.created_at);
        summaries
    }

    pub async fn list_git_accounts(&self) -> Result<GitAccountsResponse> {
        Ok(self.store.list_git_accounts().await?)
    }

    pub async fn save_git_account(
        self: &Arc<Self>,
        request: GitAccountRequest,
    ) -> Result<GitAccountResponse> {
        let account = self.store.upsert_git_account(request).await?;
        let runtime = Arc::clone(self);
        let account_id = account.id.clone();
        tokio::spawn(async move {
            if let Err(err) = runtime.verify_git_account(&account_id).await {
                tracing::warn!(account_id = %account_id, "failed to verify git account in background: {err}");
            }
        });
        Ok(GitAccountResponse { account })
    }

    pub async fn verify_git_account(&self, account_id: &str) -> Result<GitAccountSummary> {
        let token = self.git_account_token(account_id).await?;
        self.store.mark_git_account_verifying(account_id).await?;
        let response = match self
            .github_http
            .get(github_api_url(&self.github_api_base_url, "/user"))
            .bearer_auth(&token)
            .headers(github_headers())
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                return Ok(self
                    .store
                    .update_git_account_verification(
                        account_id,
                        None,
                        GitTokenKind::Unknown,
                        Vec::new(),
                        GitAccountStatus::Failed,
                        Some(redact_secret(&err.to_string(), &token)),
                    )
                    .await?);
            }
        };
        let scopes = github_scopes(response.headers());
        let token_kind = git_token_kind(&token, &scopes);
        match decode_github_response::<GithubUserApi>(response, "verify token").await {
            Ok(user) => Ok(self
                .store
                .update_git_account_verification(
                    account_id,
                    Some(user.login),
                    token_kind,
                    scopes,
                    GitAccountStatus::Verified,
                    None,
                )
                .await?),
            Err(err) => Ok(self
                .store
                .update_git_account_verification(
                    account_id,
                    None,
                    token_kind,
                    scopes,
                    GitAccountStatus::Failed,
                    Some(redact_secret(&err.to_string(), &token)),
                )
                .await?),
        }
    }

    pub async fn delete_git_account(&self, account_id: &str) -> Result<GitAccountsResponse> {
        Ok(self.store.delete_git_account(account_id).await?)
    }

    pub async fn set_default_git_account(&self, account_id: &str) -> Result<GitAccountsResponse> {
        Ok(self.store.set_default_git_account(account_id).await?)
    }

    pub async fn list_git_account_repositories(
        &self,
        account_id: &str,
    ) -> Result<GithubRepositoriesResponse> {
        let token = self.git_account_token(account_id).await?;
        let url = github_api_url(
            &self.github_api_base_url,
            "/user/repos?per_page=100&affiliation=owner,collaborator,organization_member&sort=updated",
        );
        let response = self
            .github_http
            .get(url)
            .bearer_auth(&token)
            .headers(github_headers())
            .send()
            .await?;
        let repositories: Vec<GithubRepositoryApi> =
            decode_github_response(response, "list repositories").await?;
        Ok(GithubRepositoriesResponse {
            repositories: repositories
                .into_iter()
                .map(github_repository_summary)
                .collect(),
        })
    }

    pub fn runtime_defaults(&self) -> RuntimeDefaultsResponse {
        RuntimeDefaultsResponse {
            default_docker_image: self.docker.image().to_string(),
        }
    }

    pub async fn list_git_account_repository_packages(
        &self,
        account_id: &str,
        owner: &str,
        repo: &str,
    ) -> Result<RepositoryPackagesResponse> {
        let token = self.git_account_token(account_id).await?;
        let repository_ref = format!("{}/{}", owner.trim(), repo.trim());
        if owner.trim().is_empty() || repo.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "repository owner and name are required".to_string(),
            ));
        }
        let packages = match self
            .github_container_packages_for_owner(&token, owner.trim())
            .await
        {
            Ok(packages) => packages,
            Err(err) if err.status() == Some(reqwest::StatusCode::FORBIDDEN) => {
                return Ok(RepositoryPackagesResponse {
                    packages: Vec::new(),
                    warning: Some("GitHub token cannot read packages for this owner".to_string()),
                });
            }
            Err(err) if err.status() == Some(reqwest::StatusCode::NOT_FOUND) => {
                return Ok(RepositoryPackagesResponse {
                    packages: Vec::new(),
                    warning: Some("No readable GitHub container packages found".to_string()),
                });
            }
            Err(err) => return Err(RuntimeError::Http(err)),
        };
        let mut summaries = Vec::new();
        for package in packages
            .into_iter()
            .filter(|package| github_package_belongs_to_repo(package, &repository_ref))
        {
            let versions = match self
                .github_container_package_versions(&token, owner.trim(), &package.name)
                .await
            {
                Ok(versions) => versions,
                Err(err) if err.status() == Some(reqwest::StatusCode::FORBIDDEN) => continue,
                Err(err) if err.status() == Some(reqwest::StatusCode::NOT_FOUND) => continue,
                Err(err) => return Err(RuntimeError::Http(err)),
            };
            if let Some(summary) = repository_package_summary(owner.trim(), package, versions) {
                summaries.push(summary);
            }
        }
        summaries.sort_by(|left, right| left.name.cmp(&right.name).then(left.tag.cmp(&right.tag)));
        Ok(RepositoryPackagesResponse {
            packages: summaries,
            warning: None,
        })
    }

    async fn github_container_packages_for_owner(
        &self,
        token: &str,
        owner: &str,
    ) -> std::result::Result<Vec<GithubPackageApi>, reqwest::Error> {
        let org_url = github_api_url(
            &self.github_api_base_url,
            &format!(
                "/orgs/{}/packages?package_type=container&per_page=100",
                github_path_segment(owner)
            ),
        );
        let org_response = self
            .github_http
            .get(org_url)
            .bearer_auth(token)
            .headers(github_headers())
            .send()
            .await?;
        if org_response.status() != reqwest::StatusCode::NOT_FOUND {
            return org_response.error_for_status()?.json().await;
        }
        let user_url = github_api_url(
            &self.github_api_base_url,
            &format!(
                "/users/{}/packages?package_type=container&per_page=100",
                github_path_segment(owner)
            ),
        );
        self.github_http
            .get(user_url)
            .bearer_auth(token)
            .headers(github_headers())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    async fn github_container_package_versions(
        &self,
        token: &str,
        owner: &str,
        package_name: &str,
    ) -> std::result::Result<Vec<GithubPackageVersionApi>, reqwest::Error> {
        let org_url = github_api_url(
            &self.github_api_base_url,
            &format!(
                "/orgs/{}/packages/container/{}/versions?per_page=30",
                github_path_segment(owner),
                github_path_segment(package_name)
            ),
        );
        let org_response = self
            .github_http
            .get(org_url)
            .bearer_auth(token)
            .headers(github_headers())
            .send()
            .await?;
        if org_response.status() != reqwest::StatusCode::NOT_FOUND {
            return org_response.error_for_status()?.json().await;
        }
        let user_url = github_api_url(
            &self.github_api_base_url,
            &format!(
                "/users/{}/packages/container/{}/versions?per_page=30",
                github_path_segment(owner),
                github_path_segment(package_name)
            ),
        );
        self.github_http
            .get(user_url)
            .bearer_auth(token)
            .headers(github_headers())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    pub async fn github_app_settings(&self) -> Result<GithubAppSettingsResponse> {
        Ok(self.store.get_github_app_settings().await?)
    }

    pub async fn save_github_app_settings(
        &self,
        request: GithubAppSettingsRequest,
    ) -> Result<GithubAppSettingsResponse> {
        self.github_tokens.lock().await.clear();
        Ok(self.store.save_github_app_settings(request).await?)
    }

    pub async fn start_github_app_manifest(
        &self,
        request: GithubAppManifestStartRequest,
    ) -> Result<GithubAppManifestStartResponse> {
        let origin = sanitize_origin(&request.origin)?;
        let org = match request.account_type {
            GithubAppManifestAccountType::Organization => {
                let org = request
                    .org
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput("organization is required".to_string())
                    })?;
                if !is_valid_github_slug(org) {
                    return Err(RuntimeError::InvalidInput(
                        "organization may contain only letters, numbers, or hyphens".to_string(),
                    ));
                }
                Some(org.to_string())
            }
            GithubAppManifestAccountType::Personal => None,
        };
        let state = Uuid::new_v4().to_string();
        let redirect_url = format!("{origin}/github/app-manifest/callback");
        let setup_url = format!("{origin}/github/app-installation/callback");
        let webhook_url = format!("{origin}/github/webhook-disabled");
        let manifest = github_app_manifest(&redirect_url, &setup_url, &webhook_url);
        let action_url = match (&request.account_type, &org) {
            (GithubAppManifestAccountType::Organization, Some(org)) => {
                format!(
                    "{DEFAULT_GITHUB_WEB_BASE_URL}/organizations/{org}/settings/apps/new?state={state}"
                )
            }
            _ => format!("{DEFAULT_GITHUB_WEB_BASE_URL}/settings/apps/new?state={state}"),
        };

        self.prune_github_manifest_states().await;
        self.github_manifest_states.lock().await.insert(
            state.clone(),
            GithubManifestState {
                created_at: Instant::now(),
                account_type: request.account_type,
                org,
            },
        );
        Ok(GithubAppManifestStartResponse {
            state,
            action_url,
            manifest,
        })
    }

    pub async fn complete_github_app_manifest(
        &self,
        code: &str,
        state: &str,
    ) -> Result<GithubAppSettingsResponse> {
        if !is_valid_github_manifest_code(code) {
            return Err(RuntimeError::InvalidInput(
                "invalid GitHub manifest code".to_string(),
            ));
        }
        let state_record = self.take_github_manifest_state(state).await?;
        let url = github_api_url(
            DEFAULT_GITHUB_API_BASE_URL,
            &format!("/app-manifests/{code}/conversions"),
        );
        let response = self
            .github_http
            .post(url)
            .headers(github_headers())
            .send()
            .await?;
        let conversion: GithubManifestConversionResponse =
            decode_github_response(response, "create app from manifest").await?;
        let owner_login = conversion
            .owner
            .as_ref()
            .map(|owner| owner.login.clone())
            .or_else(|| {
                state_record.org.clone().filter(|_| {
                    state_record.account_type == GithubAppManifestAccountType::Organization
                })
            });
        let owner_type = conversion
            .owner
            .as_ref()
            .map(|owner| owner.account_type.clone())
            .or_else(|| match state_record.account_type {
                GithubAppManifestAccountType::Organization => Some("Organization".to_string()),
                GithubAppManifestAccountType::Personal => Some("User".to_string()),
            });
        self.save_github_app_settings(GithubAppSettingsRequest {
            app_id: Some(conversion.id.to_string()),
            private_key: Some(conversion.pem),
            base_url: Some(DEFAULT_GITHUB_API_BASE_URL.to_string()),
            app_slug: Some(conversion.slug),
            app_html_url: Some(conversion.html_url),
            owner_login,
            owner_type,
        })
        .await
    }

    pub async fn list_github_installations(&self) -> Result<GithubInstallationsResponse> {
        let (jwt, base_url) = self.github_app_jwt().await?;
        let url = github_api_url(&base_url, "/app/installations?per_page=100");
        let response = self
            .github_http
            .get(url)
            .bearer_auth(jwt)
            .headers(github_headers())
            .send()
            .await?;
        let installations: Vec<GithubInstallationApi> =
            decode_github_response(response, "list installations").await?;
        Ok(GithubInstallationsResponse {
            installations: installations
                .into_iter()
                .map(|installation| GithubInstallationSummary {
                    id: installation.id,
                    account_login: installation.account.login,
                    account_type: installation.account.account_type,
                    repository_selection: installation.repository_selection,
                })
                .collect(),
        })
    }

    pub async fn refresh_github_installations(&self) -> Result<GithubInstallationsResponse> {
        self.github_tokens.lock().await.clear();
        self.list_github_installations().await
    }

    pub async fn list_github_repositories(
        &self,
        installation_id: u64,
    ) -> Result<GithubRepositoriesResponse> {
        if installation_id == 0 {
            return Err(RuntimeError::InvalidInput(
                "installation_id is required".to_string(),
            ));
        }
        let token = self
            .github_installation_token(installation_id, None)
            .await?;
        let (_, _, base_url) = self.github_app_secret().await?;
        let url = github_api_url(&base_url, "/installation/repositories?per_page=100");
        let response = self
            .github_http
            .get(url)
            .bearer_auth(token)
            .headers(github_headers())
            .send()
            .await?;
        let response: GithubRepositoriesApi =
            decode_github_response(response, "list installation repositories").await?;
        Ok(GithubRepositoriesResponse {
            repositories: response
                .repositories
                .into_iter()
                .map(github_repository_summary)
                .collect(),
        })
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
                    system_prompt: Some(task_role_system_prompt(AgentRole::Planner).to_string()),
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
            .create_response(
                &selection.provider,
                &selection.model,
                instructions,
                &input,
                &[],
                None,
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

    pub async fn get_project(
        &self,
        project_id: ProjectId,
        selected_agent_id: Option<AgentId>,
        session_id: Option<SessionId>,
    ) -> Result<ProjectDetail> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let agents = self.project_agents(project_id).await;
        let requested_agent_id =
            selected_agent_id.filter(|id| agents.iter().any(|agent| agent.id == *id));
        let selected_session_id = if selected_agent_id.is_some() && requested_agent_id.is_none() {
            None
        } else {
            session_id
        };
        let selected_agent_id = requested_agent_id.unwrap_or(summary.maintainer_agent_id);
        let maintainer_session_id = (selected_agent_id == summary.maintainer_agent_id)
            .then_some(selected_session_id)
            .flatten();
        let maintainer_agent = self
            .get_agent(summary.maintainer_agent_id, maintainer_session_id)
            .await?;
        let selected_agent = self
            .get_agent(selected_agent_id, selected_session_id)
            .await?;
        let status = if summary.status == ProjectStatus::Ready {
            "ready"
        } else {
            "pending"
        };
        let review_runs = self
            .list_project_review_runs(project_id, 0, PROJECT_REVIEW_RUN_LIST_LIMIT)
            .await?
            .runs;
        Ok(ProjectDetail {
            summary,
            maintainer_agent,
            agents,
            selected_agent_id,
            selected_agent,
            auth_status: status.to_string(),
            mcp_status: status.to_string(),
            review_runs,
        })
    }

    pub async fn list_project_review_runs(
        &self,
        project_id: ProjectId,
        offset: usize,
        limit: usize,
    ) -> Result<ProjectReviewRunsResponse> {
        self.project(project_id).await?;
        let since = Utc::now() - TimeDelta::days(PROJECT_REVIEW_HISTORY_RETENTION_DAYS);
        let runs = self
            .store
            .load_project_review_runs(project_id, Some(since), offset, limit)
            .await?;
        Ok(ProjectReviewRunsResponse { runs })
    }

    pub async fn get_project_review_run(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
    ) -> Result<ProjectReviewRunDetail> {
        self.project(project_id).await?;
        self.store
            .load_project_review_run(project_id, run_id)
            .await?
            .ok_or(RuntimeError::ProjectReviewRunNotFound(run_id))
    }

    pub async fn create_project(
        self: &Arc<Self>,
        request: CreateProjectRequest,
    ) -> Result<ProjectSummary> {
        let account_id = request
            .git_account_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| RuntimeError::InvalidInput("git_account_id is required".to_string()))?
            .to_string();
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
            .verified_git_account_repository(&account_id, &repository_ref)
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
        let account = self.git_account_summary(&account_id).await?;
        let installation_account = account.login.unwrap_or(account.label);
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
            installation_id: 0,
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
        self.store.save_project(&project).await?;
        self.projects.write().await.insert(
            project_id,
            Arc::new(ProjectRecord {
                summary: RwLock::new(project.clone()),
                sidecar: RwLock::new(None),
                review_worker: Mutex::new(None),
            }),
        );
        self.publish(ServiceEventKind::ProjectCreated {
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
        self.store.save_project(&updated).await?;
        self.publish(ServiceEventKind::ProjectUpdated {
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
            self.store.save_project(&summary).await?;
            self.publish(ServiceEventKind::ProjectUpdated {
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
        self.store.delete_project(project_id).await?;
        self.projects.write().await.remove(&project_id);
        self.publish(ServiceEventKind::ProjectDeleted { project_id })
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
        self.store.save_project(&updated).await?;
        self.publish(ServiceEventKind::ProjectUpdated { project: updated })
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
                self.publish(ServiceEventKind::Error {
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
            active_turn: StdMutex::new(None),
            pending_inputs: Mutex::new(VecDeque::new()),
        });

        self.agents.write().await.insert(id, Arc::clone(&agent));
        self.publish(ServiceEventKind::AgentCreated {
            agent: summary.clone(),
        })
        .await;
        Ok(agent)
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
        let (active_agent_ids, active_project_ids) = {
            let agents = self.agents.read().await;
            let projects = self.projects.read().await;
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
            .docker
            .cleanup_orphaned_managed_containers(&active_agent_ids, &active_project_ids)
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
        Ok((agent, turn_id))
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
        self.complete_turn_if_current(
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
        let agent = self.agent(agent_id).await?;
        agent.cancel_requested.store(true, Ordering::SeqCst);
        let previous_status = agent.summary.read().await.status.clone();
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
        let _ = self
            .docker
            .delete_agent_containers(&agent_id.to_string(), preferred_container_id.as_deref())
            .await?;
        {
            let mut summary = agent.summary.write().await;
            summary.status = AgentStatus::Deleted;
            summary.container_id = None;
            summary.current_turn = None;
            summary.updated_at = now();
        }
        self.persist_agent(&agent).await?;
        self.publish(ServiceEventKind::AgentStatusChanged {
            agent_id,
            status: AgentStatus::Deleted,
        })
        .await;
        Ok(previous_status)
    }

    async fn resume_agent(&self, agent_id: AgentId) -> Result<AgentSummary> {
        let agent = self.agent(agent_id).await?;
        {
            let mut summary = agent.summary.write().await;
            if summary.status == AgentStatus::Deleted {
                summary.status = AgentStatus::Idle;
                summary.last_error = None;
                summary.updated_at = now();
            }
            summary.container_id = None;
        }
        self.persist_agent(&agent).await?;
        self.ensure_agent_container(&agent, AgentStatus::Idle)
            .await?;
        Ok(agent.summary.read().await.clone())
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
        self.store.delete_task(task_id).await?;
        self.tasks.write().await.remove(&task_id);
        self.publish(ServiceEventKind::TaskDeleted { task_id })
            .await;
        Ok(())
    }

    async fn delete_agent_record(&self, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
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

        let artifact_id = Uuid::new_v4().to_string();
        let dir = self.artifact_file_dir(task_id, &artifact_id);
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

    pub fn artifact_file_path(&self, info: &ArtifactInfo) -> PathBuf {
        self.artifact_file_dir(info.task_id, &info.id)
            .join(&info.name)
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
        let result = self
            .run_turn_inner(
                agent_id,
                session_id,
                turn_id,
                message,
                skill_mentions,
                cancellation_token,
            )
            .await;
        if let Err(err) = result
            && let Ok(agent) = self.agent(agent_id).await
        {
            if matches!(err, RuntimeError::TurnCancelled) {
                let _ = self
                    .complete_turn_if_current(
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
                    .await;
                return;
            }
            let message = err.to_string();
            let completed = self
                .complete_turn_if_current(
                    &agent,
                    agent_id,
                    TurnResult {
                        turn_id,
                        status: TurnStatus::Failed,
                        agent_status: AgentStatus::Failed,
                        final_text: None,
                        error: Some(message.clone()),
                    },
                )
                .await
                .unwrap_or(false);
            if completed {
                self.publish(ServiceEventKind::Error {
                    agent_id: Some(agent_id),
                    session_id: Some(session_id),
                    turn_id: Some(turn_id),
                    message,
                })
                .await;
            }
        }
    }

    async fn run_turn_inner(
        self: &Arc<Self>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        mut skill_mentions: Vec<String>,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        let _turn_guard = agent.turn_lock.lock().await;
        let enforce_current_turn = agent.summary.read().await.current_turn == Some(turn_id);
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        self.ensure_agent_container_for_turn(
            &agent,
            AgentStatus::RunningTurn,
            turn_id,
            &cancellation_token,
        )
        .await?;
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        self.publish(ServiceEventKind::TurnStarted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
        })
        .await;

        skill_mentions.extend(extract_skill_mentions(&message));
        if let Err(err) = self
            .maybe_auto_compact(&agent, agent_id, session_id, turn_id, &cancellation_token)
            .await
        {
            if matches!(err, RuntimeError::TurnCancelled) {
                return Err(err);
            }
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
            content: message.clone(),
        })
        .await;

        let skills_config = self.store.load_skills_config().await?;
        let skills_manager = self.skills_manager_for_agent(&agent).await?;
        let skill_injections = skills_manager.build_injections_for_message(
            &message,
            &skill_mentions,
            &skills_config,
        )?;
        if !skill_injections.items.is_empty() {
            self.publish(ServiceEventKind::SkillsActivated {
                agent_id,
                session_id: Some(session_id),
                turn_id,
                skills: skill_activation_info(&skill_injections),
            })
            .await;
        }
        self.inject_project_mcp_tools(&agent, agent_id, session_id, &cancellation_token)
            .await?;
        let mut last_assistant_text: Option<String> = None;
        let mut turn_model_state = ModelTurnState::default();
        let mut empty_progress_count: usize = 0;
        loop {
            if cancellation_token.is_cancelled() {
                return Err(RuntimeError::TurnCancelled);
            }
            self.set_turn_status(
                &agent,
                turn_id,
                &cancellation_token,
                enforce_current_turn,
                AgentStatus::RunningTurn,
            )
            .await?;
            let mcp_tools = self.agent_mcp_tools(&agent).await;
            let visible_tools = self.visible_tool_names(&agent, &mcp_tools).await;
            let tools =
                build_tool_definitions_with_filter(&mcp_tools, |name| visible_tools.contains(name));
            let instructions = self
                .build_instructions(
                    &agent,
                    &skills_manager,
                    &skill_injections,
                    &skills_config,
                    &mcp_tools,
                )
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
                .maybe_auto_compact(&agent, agent_id, session_id, turn_id, &cancellation_token)
                .await
            {
                if matches!(err, RuntimeError::TurnCancelled) {
                    return Err(err);
                }
                tracing::warn!("auto context compaction failed before model request: {err}");
            }
            let mut history = self.session_history(&agent, agent_id, session_id).await?;
            if turn_model_state.previous_response_id.is_none()
                && let Some(skill_fragment) = skill_user_fragment(&skill_injections)
            {
                history.push(skill_fragment);
            }
            let response = self
                .model
                .create_turn_response_with_cancel(
                    &ModelRequest {
                        provider: &provider_selection.provider,
                        model: &provider_selection.model,
                        instructions: &instructions,
                        input: &history,
                        tools: &tools,
                        reasoning_effort,
                    },
                    &mut turn_model_state,
                    &cancellation_token,
                )
                .await
                .map_err(|err| {
                    if matches!(err, mai_model::ModelError::Cancelled) {
                        RuntimeError::TurnCancelled
                    } else {
                        RuntimeError::Model(err)
                    }
                })?;

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
            let mut made_progress = false;
            for item in response.output {
                match item {
                    ModelOutputItem::Message { text } => {
                        if !text.trim().is_empty() {
                            made_progress = true;
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
                        made_progress = true;
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
                            made_progress = true;
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
            let acknowledged_history_len = self
                .raw_session_history_len(&agent, agent_id, session_id)
                .await?;
            turn_model_state.acknowledge_history_len(acknowledged_history_len);

            if !made_progress {
                empty_progress_count = empty_progress_count.saturating_add(1);
                let diagnostic = format!(
                    "Runtime diagnostic: the previous model response produced no assistant text and no tool calls (empty_progress_count={empty_progress_count}). Decide whether to continue, ask the user for clarification, retry with a different approach, or explain the issue."
                );
                self.record_history_item(
                    &agent,
                    agent_id,
                    session_id,
                    ModelInputItem::user_text(diagnostic),
                )
                .await?;
                continue;
            }
            empty_progress_count = 0;

            if tool_calls.is_empty() {
                self.finish_turn(
                    &agent,
                    agent_id,
                    session_id,
                    TurnResult {
                        turn_id,
                        status: TurnStatus::Completed,
                        agent_status: AgentStatus::Completed,
                        final_text: last_assistant_text,
                        error: None,
                    },
                )
                .await?;
                return Ok(());
            }

            self.set_turn_status(
                &agent,
                turn_id,
                &cancellation_token,
                enforce_current_turn,
                AgentStatus::WaitingTool,
            )
            .await?;
            let mut should_end_turn = false;
            for (call_id, name, arguments) in tool_calls {
                if cancellation_token.is_cancelled() {
                    return Err(RuntimeError::TurnCancelled);
                }
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
                    .execute_tool(
                        &agent,
                        agent_id,
                        turn_id,
                        &name,
                        arguments,
                        cancellation_token.clone(),
                    )
                    .await;
                let duration_ms = u128_to_u64(started_at.elapsed().as_millis());
                let execution = match output {
                    Ok(execution) => execution,
                    Err(RuntimeError::TurnCancelled) => return Err(RuntimeError::TurnCancelled),
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
                if cancellation_token.is_cancelled() {
                    return Err(RuntimeError::TurnCancelled);
                }
            }

            if should_end_turn {
                self.finish_turn(
                    &agent,
                    agent_id,
                    session_id,
                    TurnResult {
                        turn_id,
                        status: TurnStatus::Completed,
                        agent_status: AgentStatus::Completed,
                        final_text: last_assistant_text,
                        error: None,
                    },
                )
                .await?;
                return Ok(());
            }
        }
    }

    async fn execute_tool(
        self: &Arc<Self>,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        _turn_id: TurnId,
        name: &str,
        arguments: Value,
        cancellation_token: CancellationToken,
    ) -> Result<ToolExecution> {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        self.check_tool_permission(agent, name, &arguments).await?;
        match route_tool(name) {
            RoutedTool::ContainerExec => {
                let command = required_string(&arguments, "command")?;
                let cwd = optional_string(&arguments, "cwd");
                let timeout = arguments.get("timeout_secs").and_then(Value::as_u64);
                let container_id = self.container_id(agent_id).await?;
                let output = self
                    .docker
                    .exec_shell_with_cancel(
                        &container_id,
                        &command,
                        cwd.as_deref(),
                        timeout,
                        &cancellation_token,
                    )
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
                let collab_input = collab_input_from_args(&arguments)?;
                let legacy_role = optional_string(&arguments, "role")
                    .as_deref()
                    .map(parse_agent_role)
                    .transpose()?;
                let role = legacy_role
                    .or_else(|| {
                        optional_string(&arguments, "agent_type")
                            .and_then(|value| agent_type_role(&value))
                    })
                    .unwrap_or_default();
                let parent = self.agent(agent_id).await?;
                let parent_status = parent.summary.read().await.status.clone();
                let parent_summary = parent.summary.read().await.clone();
                let parent_container_id =
                    self.ensure_agent_container(&parent, parent_status).await?;
                let parent_docker_image = parent_summary.docker_image.clone();
                let (provider_id, model, reasoning_effort) = if legacy_role.is_some() {
                    let child_model = self.resolve_role_agent_model(role).await?;
                    (
                        child_model.preference.provider_id,
                        child_model.preference.model,
                        child_model.preference.reasoning_effort,
                    )
                } else {
                    (
                        parent_summary.provider_id.clone(),
                        optional_string(&arguments, "model")
                            .unwrap_or_else(|| parent_summary.model.clone()),
                        optional_string(&arguments, "reasoning_effort")
                            .or_else(|| parent_summary.reasoning_effort.clone()),
                    )
                };
                let created = self
                    .create_agent_with_container_source(
                        CreateAgentRequest {
                            name,
                            provider_id: Some(provider_id),
                            model: Some(model),
                            reasoning_effort,
                            docker_image: Some(parent_docker_image.clone()),
                            parent_id: Some(agent_id),
                            system_prompt: Some(task_role_system_prompt(role).to_string()),
                        },
                        ContainerSource::CloneFrom {
                            parent_container_id,
                            docker_image: parent_docker_image,
                            workspace_volume: None,
                        },
                        parent_summary.task_id,
                        parent_summary.project_id,
                        Some(role),
                    )
                    .await?;
                if arguments
                    .get("fork_context")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    self.fork_agent_context(agent_id, created.id).await?;
                }
                let turn_id = if let Some(message) = collab_input.message {
                    let session_id = self.resolve_session_id(created.id, None).await?;
                    let (agent, turn_id) = self.prepare_turn(created.id).await?;
                    self.spawn_turn(
                        &agent,
                        created.id,
                        session_id,
                        turn_id,
                        message,
                        collab_input.skill_mentions,
                    );
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
            RoutedTool::SendInput => {
                let target =
                    parse_agent_id(&required_any_string(&arguments, &["target", "agent_id"])?)?;
                let collab_input = collab_input_from_args(&arguments)?;
                let message = collab_input.message.ok_or_else(|| {
                    RuntimeError::InvalidInput(
                        "send_input requires message or text items".to_string(),
                    )
                })?;
                let interrupt = arguments
                    .get("interrupt")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let output = self
                    .send_input_to_agent(
                        target,
                        None,
                        message,
                        collab_input.skill_mentions,
                        interrupt,
                    )
                    .await?;
                Ok(ToolExecution {
                    success: true,
                    output: output.to_string(),
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
                let output = self
                    .send_input_to_agent(target, session_id, message, Vec::new(), false)
                    .await?;
                Ok(ToolExecution {
                    success: true,
                    output: output.to_string(),
                    ends_turn: false,
                })
            }
            RoutedTool::WaitAgent => {
                let legacy_single_target =
                    arguments.get("targets").is_none() && arguments.get("agent_id").is_some();
                let targets = wait_targets(&arguments)?;
                let timeout = wait_timeout(&arguments);
                if legacy_single_target && targets.len() == 1 {
                    let output = self
                        .wait_agents_output_with_cancel(targets, timeout, &cancellation_token)
                        .await?;
                    return Ok(ToolExecution {
                        success: true,
                        output: serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string()),
                        ends_turn: false,
                    });
                }
                let output = self
                    .wait_agents_output_with_cancel(targets, timeout, &cancellation_token)
                    .await?;
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
                let target =
                    parse_agent_id(&required_any_string(&arguments, &["target", "agent_id"])?)?;
                let previous = self.close_agent(target).await?;
                Ok(ToolExecution {
                    success: true,
                    output: json!({ "closed": target, "previous_status": previous }).to_string(),
                    ends_turn: false,
                })
            }
            RoutedTool::ResumeAgent => {
                let target = parse_agent_id(&required_any_string(
                    &arguments,
                    &["id", "agent_id", "target"],
                )?)?;
                let resumed = self.resume_agent(target).await?;
                Ok(ToolExecution {
                    success: true,
                    output: json!({ "agent": resumed }).to_string(),
                    ends_turn: false,
                })
            }
            RoutedTool::ListMcpResources => {
                let manager = self
                    .mcp_manager_for_tool(agent, agent_id, &cancellation_token)
                    .await?;
                let output = manager
                    .list_resources(
                        optional_string(&arguments, "server").as_deref(),
                        optional_string(&arguments, "cursor"),
                    )
                    .await?;
                Ok(ToolExecution {
                    success: true,
                    output: output.to_string(),
                    ends_turn: false,
                })
            }
            RoutedTool::ListMcpResourceTemplates => {
                let manager = self
                    .mcp_manager_for_tool(agent, agent_id, &cancellation_token)
                    .await?;
                let output = manager
                    .list_resource_templates(
                        optional_string(&arguments, "server").as_deref(),
                        optional_string(&arguments, "cursor"),
                    )
                    .await?;
                Ok(ToolExecution {
                    success: true,
                    output: output.to_string(),
                    ends_turn: false,
                })
            }
            RoutedTool::ReadMcpResource => {
                let manager = self
                    .mcp_manager_for_tool(agent, agent_id, &cancellation_token)
                    .await?;
                let server = required_string(&arguments, "server")?;
                let uri = required_string(&arguments, "uri")?;
                let output = manager.read_resource(&server, &uri).await?;
                Ok(ToolExecution {
                    success: true,
                    output: output.to_string(),
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
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput("missing boolean field `passed`".to_string())
                    })?;
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
                let items_arg = arguments.get("items").ok_or_else(|| {
                    RuntimeError::InvalidInput("missing field `items`".to_string())
                })?;
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
                let questions_arg = arguments.get("questions").ok_or_else(|| {
                    RuntimeError::InvalidInput("missing field `questions`".to_string())
                })?;
                let raw_questions: Vec<serde_json::Value> =
                    serde_json::from_value(questions_arg.clone()).map_err(|e| {
                        RuntimeError::InvalidInput(format!("invalid questions: {e}"))
                    })?;
                let mut questions = Vec::with_capacity(raw_questions.len());
                for raw in &raw_questions {
                    let id = raw
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let question = raw
                        .get("question")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let options_raw = raw
                        .get("options")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let mut options = Vec::with_capacity(options_raw.len());
                    for opt in &options_raw {
                        let label = opt
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let description = opt
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        options.push(UserInputOption { label, description });
                    }
                    questions.push(UserInputQuestion {
                        id,
                        question,
                        options,
                    });
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
                    output: "Questions sent to user. Wait for their response in the next message."
                        .to_string(),
                    ends_turn: true,
                })
            }
            RoutedTool::SaveArtifact => {
                let path = required_string(&arguments, "path")?;
                let name = optional_string(&arguments, "name");
                let artifact = self.save_artifact(agent_id, path, name).await?;
                Ok(ToolExecution {
                    success: true,
                    output: serde_json::to_string(&artifact).unwrap_or_else(|_| "{}".to_string()),
                    ends_turn: false,
                })
            }
            RoutedTool::GithubApiGet => {
                let path = required_string(&arguments, "path")?;
                self.execute_project_github_api_get(agent, &path).await
            }
            RoutedTool::Mcp(model_name) => {
                if agent.summary.read().await.project_id.is_some() {
                    return self
                        .execute_project_mcp_tool(agent, &model_name, arguments, cancellation_token)
                        .await;
                }
                let manager = agent.mcp.read().await.clone().ok_or_else(|| {
                    RuntimeError::InvalidInput("MCP manager not initialized".to_string())
                })?;
                let output = tokio::select! {
                    output = manager.call_model_tool(&model_name, arguments) => output?,
                    _ = cancellation_token.cancelled() => {
                        return Err(RuntimeError::TurnCancelled);
                    }
                };
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
                system_prompt: Some(task_role_system_prompt(role).to_string()),
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
        self.wait_agent_with_cancel(agent_id, timeout, &CancellationToken::new())
            .await
    }

    async fn wait_agent_with_cancel(
        &self,
        agent_id: AgentId,
        timeout: Duration,
        cancellation_token: &CancellationToken,
    ) -> Result<AgentSummary> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancellation_token.is_cancelled() {
                return Err(RuntimeError::TurnCancelled);
            }
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
            tokio::select! {
                _ = sleep(Duration::from_millis(250)) => {},
                _ = cancellation_token.cancelled() => return Err(RuntimeError::TurnCancelled),
            }
        }
    }

    async fn wait_agent_until_complete_with_cancel(
        &self,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> Result<AgentSummary> {
        loop {
            if cancellation_token.is_cancelled() {
                return Err(RuntimeError::TurnCancelled);
            }
            let agent = self.agent(agent_id).await?;
            let summary = agent.summary.read().await.clone();
            if Self::is_agent_wait_complete(&summary) {
                return Ok(summary);
            }
            tokio::select! {
                _ = sleep(Duration::from_millis(250)) => {},
                _ = cancellation_token.cancelled() => return Err(RuntimeError::TurnCancelled),
            }
        }
    }

    fn is_agent_wait_complete(summary: &AgentSummary) -> bool {
        summary.current_turn.is_none()
            || matches!(
                summary.status,
                AgentStatus::Completed
                    | AgentStatus::Failed
                    | AgentStatus::Cancelled
                    | AgentStatus::Deleted
                    | AgentStatus::Idle
            )
    }

    async fn agent_wait_snapshot(&self, agent_id: AgentId) -> Result<Value> {
        let agent = self.agent(agent_id).await?;
        let summary = agent.summary.read().await.clone();
        let (session_id, recent_messages) = self.agent_recent_messages(agent_id, 12).await?;
        let last_message = recent_messages.last().cloned();
        let tracked_response = {
            let sessions = agent.sessions.lock().await;
            sessions
                .iter()
                .filter_map(|s| s.last_turn_response.as_ref())
                .next_back()
                .cloned()
        };
        let final_response = if Self::is_agent_wait_complete(&summary) {
            tracked_response.or_else(|| {
                recent_messages
                    .iter()
                    .rev()
                    .find(|message| message.role == MessageRole::Assistant)
                    .map(|message| message.content.clone())
            })
        } else {
            None
        };
        let recent_events = self.agent_recent_events(agent_id, 12).await;
        let last_activity_at = last_activity_at(&summary, &recent_messages, &recent_events);
        let active_tool = active_tool_snapshot(&recent_events);
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
        let events = self.recent_events.lock().await;
        let mut selected = events
            .iter()
            .rev()
            .filter(|event| event_agent_id(event) == Some(agent_id))
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        selected.reverse();
        selected
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
                if Self::is_agent_wait_complete(&summary) {
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

    async fn agent_capability(&self, agent: &AgentRecord) -> AgentCapability {
        let summary = agent.summary.read().await.clone();
        let is_project_maintainer = if let Some(project_id) = summary.project_id {
            let project = self.projects.read().await.get(&project_id).cloned();
            if let Some(project) = project {
                project.summary.read().await.maintainer_agent_id == summary.id
            } else {
                false
            }
        } else {
            summary.parent_id.is_none()
        };
        if is_project_maintainer || summary.parent_id.is_none() {
            AgentCapability {
                can_spawn_agents: true,
                can_close_agents: true,
                communication: AgentCommunicationPolicy::All,
            }
        } else {
            AgentCapability {
                can_spawn_agents: false,
                can_close_agents: false,
                communication: AgentCommunicationPolicy::ParentAndMaintainer,
            }
        }
    }

    async fn agent_can_access_target(&self, agent: &AgentRecord, target: AgentId) -> bool {
        let capability = self.agent_capability(agent).await;
        if capability.communication == AgentCommunicationPolicy::All {
            return true;
        }
        let summary = agent.summary.read().await.clone();
        if summary.parent_id == Some(target) {
            return true;
        }
        let Some(project_id) = summary.project_id else {
            return false;
        };
        let project = self.projects.read().await.get(&project_id).cloned();
        if let Some(project) = project {
            project.summary.read().await.maintainer_agent_id == target
        } else {
            false
        }
    }

    async fn check_tool_permission(
        &self,
        agent: &AgentRecord,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<()> {
        let capability = self.agent_capability(agent).await;
        match route_tool(tool_name) {
            RoutedTool::SpawnAgent if !capability.can_spawn_agents => {
                return Err(RuntimeError::InvalidInput(
                    "Tool 'spawn_agent' is not available for worker agents".to_string(),
                ));
            }
            RoutedTool::CloseAgent if !capability.can_close_agents => {
                return Err(RuntimeError::InvalidInput(
                    "Tool 'close_agent' is not available for worker agents".to_string(),
                ));
            }
            RoutedTool::SendInput | RoutedTool::SendMessage => {
                let target = match route_tool(tool_name) {
                    RoutedTool::SendInput => {
                        parse_agent_id(&required_any_string(arguments, &["target", "agent_id"])?)?
                    }
                    RoutedTool::SendMessage => {
                        parse_agent_id(&required_string(arguments, "agent_id")?)?
                    }
                    _ => unreachable!(),
                };
                if !self.agent_can_access_target(agent, target).await {
                    return Err(RuntimeError::InvalidInput(
                        "target agent is outside this agent's communication policy".to_string(),
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn visible_tool_names(
        &self,
        agent: &AgentRecord,
        mcp_tools: &[McpTool],
    ) -> HashSet<String> {
        let capability = self.agent_capability(agent).await;
        let mut names = HashSet::from([
            mai_tools::TOOL_CONTAINER_EXEC.to_string(),
            mai_tools::TOOL_CONTAINER_CP_UPLOAD.to_string(),
            mai_tools::TOOL_CONTAINER_CP_DOWNLOAD.to_string(),
            mai_tools::TOOL_SEND_INPUT.to_string(),
            mai_tools::TOOL_SEND_MESSAGE.to_string(),
            mai_tools::TOOL_WAIT_AGENT.to_string(),
            mai_tools::TOOL_LIST_AGENTS.to_string(),
            mai_tools::TOOL_RESUME_AGENT.to_string(),
            mai_tools::TOOL_LIST_MCP_RESOURCES.to_string(),
            mai_tools::TOOL_LIST_MCP_RESOURCE_TEMPLATES.to_string(),
            mai_tools::TOOL_READ_MCP_RESOURCE.to_string(),
            mai_tools::TOOL_SAVE_TASK_PLAN.to_string(),
            mai_tools::TOOL_SUBMIT_REVIEW_RESULT.to_string(),
            mai_tools::TOOL_UPDATE_TODO_LIST.to_string(),
            mai_tools::TOOL_REQUEST_USER_INPUT.to_string(),
            mai_tools::TOOL_SAVE_ARTIFACT.to_string(),
            mai_tools::TOOL_GITHUB_API_GET.to_string(),
        ]);
        if capability.can_spawn_agents {
            names.insert(mai_tools::TOOL_SPAWN_AGENT.to_string());
        }
        if capability.can_close_agents {
            names.insert(mai_tools::TOOL_CLOSE_AGENT.to_string());
        }
        names.extend(mcp_tools.iter().map(|tool| tool.model_name.clone()));
        names
    }

    async fn send_input_to_agent(
        self: &Arc<Self>,
        target: AgentId,
        session_id: Option<SessionId>,
        message: String,
        skill_mentions: Vec<String>,
        interrupt: bool,
    ) -> Result<Value> {
        let agent = self.agent(target).await?;
        if interrupt {
            let current_turn = agent.summary.read().await.current_turn;
            if let Some(turn_id) = current_turn {
                self.cancel_agent_turn(target, turn_id).await?;
            } else {
                agent.cancel_requested.store(true, Ordering::SeqCst);
                self.set_status(&agent, AgentStatus::Cancelled, None)
                    .await?;
            }
            self.wait_agent(target, TURN_CANCEL_GRACE).await?;
        }
        match self.prepare_turn(target).await {
            Ok((agent, turn_id)) => {
                let session_id = self.resolve_session_id(target, session_id).await?;
                self.spawn_turn(&agent, target, session_id, turn_id, message, skill_mentions);
                Ok(json!({ "turn_id": turn_id, "queued": false }))
            }
            Err(RuntimeError::AgentBusy(_)) if !interrupt => {
                agent
                    .pending_inputs
                    .lock()
                    .await
                    .push_back(QueuedAgentInput {
                        session_id,
                        message,
                        skill_mentions,
                    });
                Ok(json!({ "queued": true }))
            }
            Err(err) => Err(err),
        }
    }

    async fn start_next_queued_input(self: &Arc<Self>, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        let Some(input) = agent.pending_inputs.lock().await.pop_front() else {
            return Ok(());
        };
        let session_id = self.resolve_session_id(agent_id, input.session_id).await?;
        let (agent, turn_id) = match self.prepare_turn(agent_id).await {
            Ok(turn) => turn,
            Err(RuntimeError::AgentBusy(_)) => {
                agent.pending_inputs.lock().await.push_front(input);
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        self.spawn_turn(
            &agent,
            agent_id,
            session_id,
            turn_id,
            input.message,
            input.skill_mentions,
        );
        Ok(())
    }

    async fn fork_agent_context(&self, parent_id: AgentId, child_id: AgentId) -> Result<()> {
        let parent = self.agent(parent_id).await?;
        let child = self.agent(child_id).await?;
        let parent_session = {
            let sessions = parent.sessions.lock().await;
            selected_session(&sessions, None).cloned()
        }
        .ok_or(RuntimeError::AgentNotFound(parent_id))?;
        let child_session_id = self.resolve_session_id(child_id, None).await?;
        {
            let mut child_sessions = child.sessions.lock().await;
            let child_session = child_sessions
                .iter_mut()
                .find(|session| session.summary.id == child_session_id)
                .ok_or(RuntimeError::SessionNotFound {
                    agent_id: child_id,
                    session_id: child_session_id,
                })?;
            child_session.messages = parent_session.messages.clone();
            child_session.history = parent_session.history.clone();
            child_session.summary.message_count = child_session.messages.len();
            child_session.summary.updated_at = now();
            let summary = child_session.summary.clone();
            self.store.save_agent_session(child_id, &summary).await?;
        }
        self.store
            .replace_agent_history(child_id, child_session_id, &parent_session.history)
            .await?;
        for (position, message) in parent_session.messages.iter().enumerate() {
            self.store
                .append_agent_message(child_id, child_session_id, position, message)
                .await?;
        }
        Ok(())
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
        Ok((Some(session.summary.id), session.messages[start..].to_vec()))
    }

    async fn build_instructions(
        &self,
        agent: &AgentRecord,
        skills_manager: &SkillsManager,
        skill_injections: &SkillInjections,
        skills_config: &SkillsConfigRequest,
        mcp_tools: &[mai_mcp::McpTool],
    ) -> Result<String> {
        let mut instructions = String::from(BASE_INSTRUCTIONS);
        if let Some(system_prompt) = &agent.system_prompt {
            instructions.push_str("\n\n## Agent System Prompt\n");
            instructions.push_str(system_prompt);
        }
        instructions.push_str("\n\n## Available Skills\n");
        instructions.push_str(&skills_manager.render_available(skills_config)?);
        if !skill_injections.warnings.is_empty() {
            instructions.push_str("\n\n## Skill Warnings\n");
            for warning in &skill_injections.warnings {
                instructions.push_str(&format!("\n- {warning}"));
            }
        }
        if !skill_injections.suggestions.is_empty() {
            instructions.push_str("\n\n## Skill Suggestions for This Turn\n");
            instructions.push_str(
                "The following skills look potentially relevant but were not injected. Use them only if they fit the current task; read the path before relying on one.",
            );
            for suggestion in &skill_injections.suggestions {
                let display = suggestion
                    .display_name
                    .as_deref()
                    .unwrap_or(&suggestion.name);
                instructions.push_str(&format!(
                    "\n- ${name} ({display}): {description} (reason: {reason}; path: {path})",
                    name = suggestion.name,
                    display = display,
                    description = suggestion.description,
                    reason = suggestion.reason,
                    path = suggestion.path.display()
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
        self: &Arc<Self>,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        result: TurnResult,
    ) -> Result<()> {
        let _ = session_id;
        self.complete_turn_if_current(agent, agent_id, result)
            .await?;
        Ok(())
    }

    async fn complete_turn_if_current(
        self: &Arc<Self>,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        result: TurnResult,
    ) -> Result<bool> {
        let turn_id = result.turn_id;
        let session_id = {
            let mut active_turn = agent.active_turn.lock().expect("active turn lock");
            let active_session_id = active_turn
                .as_ref()
                .filter(|turn| turn.turn_id == turn_id)
                .map(|turn| turn.session_id);
            if active_session_id.is_some() {
                *active_turn = None;
            }
            active_session_id
        };
        let session_id = match session_id {
            Some(session_id) => session_id,
            None => {
                let current_turn = agent.summary.read().await.current_turn;
                if current_turn != Some(turn_id) {
                    return Ok(false);
                }
                // Legacy in-memory records may not have active_turn populated; keep the turn's selected session.
                agent
                    .sessions
                    .lock()
                    .await
                    .first()
                    .map(|session| session.summary.id)
                    .ok_or(RuntimeError::TurnNotFound { agent_id, turn_id })?
            }
        };
        {
            let mut summary = agent.summary.write().await;
            if summary.current_turn != Some(turn_id) {
                return Ok(false);
            }
            summary.status = result.agent_status.clone();
            summary.current_turn = None;
            summary.updated_at = now();
            if let Some(error) = result.error {
                summary.last_error = Some(error);
            }
        }
        {
            let mut sessions = agent.sessions.lock().await;
            if let Some(session) = sessions.iter_mut().find(|s| s.summary.id == session_id) {
                session.last_turn_response = result.final_text;
            }
        }
        self.persist_agent(agent).await?;
        self.publish(ServiceEventKind::TurnCompleted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
            status: result.status,
        })
        .await;
        self.publish(ServiceEventKind::AgentStatusChanged {
            agent_id,
            status: result.agent_status,
        })
        .await;
        if let Err(err) = self.start_next_queued_input(agent_id).await {
            tracing::warn!("failed to start queued agent input: {err}");
        }
        Ok(true)
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
        cancellation_token: &CancellationToken,
    ) -> Result<()> {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
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
        let skills_config = self.store.load_skills_config().await?;
        let skills_manager = self.skills_manager_for_agent(agent).await?;
        let instructions = self
            .build_instructions(
                agent,
                &skills_manager,
                &SkillInjections::default(),
                &skills_config,
                &[],
            )
            .await?;
        let response = self
            .model
            .create_response_with_cancel(
                &ModelRequest {
                    provider: &provider_selection.provider,
                    model: &provider_selection.model,
                    instructions: &instructions,
                    input: &compact_input,
                    tools: &[],
                    reasoning_effort: summary.reasoning_effort,
                },
                cancellation_token,
            )
            .await
            .map_err(|err| {
                if matches!(err, mai_model::ModelError::Cancelled) {
                    RuntimeError::TurnCancelled
                } else {
                    RuntimeError::Model(err)
                }
            })?;

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

    async fn project(&self, project_id: ProjectId) -> Result<Arc<ProjectRecord>> {
        self.projects
            .read()
            .await
            .get(&project_id)
            .cloned()
            .ok_or(RuntimeError::ProjectNotFound(project_id))
    }

    async fn project_agents(&self, project_id: ProjectId) -> Vec<AgentSummary> {
        let agents = self.agents.read().await;
        let mut summaries = Vec::new();
        for agent in agents.values() {
            let summary = agent.summary.read().await.clone();
            if summary.project_id == Some(project_id) {
                summaries.push(summary);
            }
        }
        summaries.sort_by_key(|summary| summary.created_at);
        summaries
    }

    async fn project_auto_reviewer_agents(&self, project_id: ProjectId) -> Vec<AgentSummary> {
        let maintainer_agent_id = match self.project(project_id).await {
            Ok(project) => project.summary.read().await.maintainer_agent_id,
            Err(_) => return Vec::new(),
        };
        self.project_agents(project_id)
            .await
            .into_iter()
            .filter(|summary| {
                summary.role == Some(AgentRole::Reviewer)
                    && summary.parent_id == Some(maintainer_agent_id)
                    && !summary.status.is_terminal()
            })
            .collect()
    }

    async fn project_skills_from_cache(&self, project_id: ProjectId) -> Result<SkillsListResponse> {
        let config = self.store.load_skills_config().await?;
        let mut response =
            SkillsManager::with_roots(self.project_skill_roots(project_id)).list(&config)?;
        self.apply_project_skill_source_paths(project_id, &mut response);
        Ok(response)
    }

    fn skills_manager_with_project_roots(&self, project_id: ProjectId) -> SkillsManager {
        self.skills
            .clone_with_extra_roots(self.project_skill_roots(project_id))
    }

    async fn skills_manager_for_agent(&self, agent: &AgentRecord) -> Result<SkillsManager> {
        let project_id = agent.summary.read().await.project_id;
        Ok(project_id
            .map(|project_id| self.skills_manager_with_project_roots(project_id))
            .unwrap_or_else(|| self.skills.clone()))
    }

    fn project_skill_cache_dir(&self, project_id: ProjectId) -> PathBuf {
        self.cache_root
            .join(PROJECT_SKILLS_CACHE_DIR)
            .join(project_id.to_string())
    }

    fn artifact_file_dir(&self, task_id: TaskId, artifact_id: &str) -> PathBuf {
        self.artifact_files_root
            .join(task_id.to_string())
            .join(artifact_id)
    }

    fn project_skill_roots(&self, project_id: ProjectId) -> Vec<(PathBuf, SkillScope)> {
        let cache_dir = self.project_skill_cache_dir(project_id);
        PROJECT_SKILL_CANDIDATE_DIRS
            .iter()
            .map(|(_, cache_name)| (cache_dir.join(cache_name), SkillScope::Project))
            .collect()
    }

    fn apply_project_skill_source_paths(
        &self,
        project_id: ProjectId,
        response: &mut SkillsListResponse,
    ) {
        let cache_dir = self.project_skill_cache_dir(project_id);
        for skill in &mut response.skills {
            if skill.scope != SkillScope::Project {
                continue;
            }
            if let Some(source_path) = project_skill_source_path(&cache_dir, &skill.path) {
                skill.source_path = Some(source_path);
            }
        }
        for error in &mut response.errors {
            if let Some(source_path) = project_skill_source_path(&cache_dir, &error.path) {
                error.path = source_path;
            }
        }
        response.roots = PROJECT_SKILL_CANDIDATE_DIRS
            .iter()
            .filter_map(|(relative, cache_name)| {
                let root = cache_dir.join(cache_name);
                root.exists()
                    .then(|| PathBuf::from(PROJECT_WORKSPACE_PATH).join(relative))
            })
            .collect();
    }

    async fn existing_project_skill_dirs(
        &self,
        container_id: &str,
    ) -> Result<Vec<ProjectSkillSourceDir>> {
        let checks = PROJECT_SKILL_CANDIDATE_DIRS
            .iter()
            .map(|(relative, cache_name)| {
                let container_path = format!("{PROJECT_WORKSPACE_PATH}/{relative}");
                format!(
                    "if [ -d {path} ]; then printf '%s\\t%s\\t%s\\n' {relative} {cache_name} {path}; fi",
                    path = shell_quote(&container_path),
                    relative = shell_quote(relative),
                    cache_name = shell_quote(cache_name),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let output = self
            .docker
            .exec_shell(container_id, &checks, Some("/"), Some(20))
            .await?;
        if output.status != 0 {
            let combined = format!("{}\n{}", output.stderr, output.stdout);
            let message = preview(combined.trim(), 500);
            return Err(RuntimeError::InvalidInput(format!(
                "project skill directory detection failed: {message}"
            )));
        }
        Ok(output
            .stdout
            .lines()
            .filter_map(ProjectSkillSourceDir::from_line)
            .collect())
    }

    async fn refresh_project_skill_cache(
        &self,
        project_id: ProjectId,
        container_id: &str,
        sources: &[ProjectSkillSourceDir],
    ) -> Result<()> {
        let cache_dir = self.project_skill_cache_dir(project_id);
        if cache_dir.exists() {
            fs::remove_dir_all(&cache_dir)?;
        }
        fs::create_dir_all(&cache_dir)?;
        for source in sources {
            let target = cache_dir.join(&source.cache_name);
            self.docker
                .copy_from_container_to_file(container_id, &source.container_path, &target)
                .await?;
            normalize_copied_project_skill_dir(&target, &source.cache_name)?;
        }
        Ok(())
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

        let workspace_volume = DockerClient::workspace_volume_for_project(&project_id.to_string());
        let container = self
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
        let configs = project_mcp_configs(&token);
        self.publish(ServiceEventKind::McpServerStatusChanged {
            agent_id,
            server: "project".to_string(),
            status: mai_protocol::McpStartupStatus::Starting,
            error: None,
        })
        .await;
        let manager = McpAgentManager::start(self.docker.clone(), sidecar.id, configs).await;
        if cancellation_token.is_cancelled() {
            manager.shutdown().await;
            return Err(RuntimeError::TurnCancelled);
        }
        for status in manager.statuses().await {
            let error = status.error.map(|error| redact_secret(&error, &token));
            self.publish(ServiceEventKind::McpServerStatusChanged {
                agent_id,
                server: status.server,
                status: status.status,
                error,
            })
            .await;
        }
        let manager = Arc::new(manager);
        let mut managers = self.project_mcp_managers.write().await;
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
        Ok(Some(self.git_account_token(&account_id).await?))
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
        if let Some(manager) = self.project_mcp_managers.write().await.remove(&project_id) {
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
        self.store.save_project(&updated).await?;
        self.publish(ServiceEventKind::ProjectUpdated {
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
            let projects = self.projects.read().await;
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
            let projects = self.projects.read().await;
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
            let _ = self
                .finish_project_review_run(
                    run.id,
                    project_id,
                    run.reviewer_agent_id,
                    run.turn_id,
                    ProjectReviewRunStatus::Cancelled,
                    None,
                    run.pr,
                    run.summary,
                    Some("review interrupted by server restart".to_string()),
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
                .set_project_review_state(project_id, status, None, None, None, None, None, false)
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
        let _ = self
            .cancel_active_project_review_runs(project_id, reviewer_id)
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
                None,
                None,
                None,
                None,
                None,
                true,
            )
            .await;
    }

    async fn run_project_review_loop(
        self: Arc<Self>,
        project_id: ProjectId,
        cancellation_token: CancellationToken,
    ) {
        if let Err(err) = self.ensure_project_review_workspace(project_id).await {
            let _ = self
                .record_project_review_startup_failure(project_id, err.to_string())
                .await;
            let next = Utc::now() + TimeDelta::seconds(PROJECT_REVIEW_FAILURE_RETRY_SECS as i64);
            let _ = self
                .set_project_review_state(
                    project_id,
                    ProjectReviewStatus::Failed,
                    None,
                    Some(next),
                    None,
                    None,
                    Some(err.to_string()),
                    false,
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
                .run_project_review_once(project_id, cancellation_token.clone())
                .await;
            let decision = match decision {
                Ok(result) => project_review_loop_decision_for_result(result),
                Err(RuntimeError::TurnCancelled) if cancellation_token.is_cancelled() => break,
                Err(err) => project_review_loop_decision_for_error(err.to_string()),
            };
            let next_review_at = (decision.delay.as_secs() > 0)
                .then(|| Utc::now() + TimeDelta::seconds(decision.delay.as_secs() as i64));
            let _ = self
                .set_project_review_state(
                    project_id,
                    decision.status,
                    None,
                    next_review_at,
                    decision.outcome,
                    decision.summary,
                    decision.error,
                    false,
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
    ) -> Result<ProjectReviewCycleResult> {
        let run_id = Uuid::new_v4();
        self.set_project_review_state(
            project_id,
            ProjectReviewStatus::Syncing,
            None,
            None,
            None,
            None,
            None,
            false,
        )
        .await?;
        self.save_project_review_run_status(
            ProjectReviewRunSummary {
                id: run_id,
                project_id,
                reviewer_agent_id: None,
                turn_id: None,
                started_at: now(),
                finished_at: None,
                status: ProjectReviewRunStatus::Syncing,
                outcome: None,
                pr: None,
                summary: None,
                error: None,
            },
            Vec::new(),
            Vec::new(),
        )
        .await?;
        if let Err(err) = self.sync_project_review_repo(project_id).await {
            let error = err.to_string();
            self.finish_project_review_run(
                run_id,
                project_id,
                None,
                None,
                ProjectReviewRunStatus::Failed,
                Some(ProjectReviewOutcome::Failed),
                None,
                None,
                Some(error),
            )
            .await?;
            return Err(err);
        }
        if cancellation_token.is_cancelled() {
            self.finish_project_review_run(
                run_id,
                project_id,
                None,
                None,
                ProjectReviewRunStatus::Cancelled,
                None,
                None,
                None,
                Some("review cancelled".to_string()),
            )
            .await?;
            return Err(RuntimeError::TurnCancelled);
        }
        let reviewer = match self.spawn_project_reviewer_agent(project_id).await {
            Ok(reviewer) => reviewer,
            Err(err) => {
                self.finish_project_review_run(
                    run_id,
                    project_id,
                    None,
                    None,
                    ProjectReviewRunStatus::Failed,
                    Some(ProjectReviewOutcome::Failed),
                    None,
                    None,
                    Some(err.to_string()),
                )
                .await?;
                return Err(err);
            }
        };
        let reviewer_id = reviewer.id;
        self.set_project_review_state(
            project_id,
            ProjectReviewStatus::Running,
            Some(reviewer_id),
            None,
            None,
            None,
            None,
            false,
        )
        .await?;
        let started_at = self
            .store
            .load_project_review_run(project_id, run_id)
            .await?
            .map(|run| run.summary.started_at)
            .unwrap_or_else(now);
        self.save_project_review_run_status(
            ProjectReviewRunSummary {
                id: run_id,
                project_id,
                reviewer_agent_id: Some(reviewer_id),
                turn_id: None,
                started_at,
                finished_at: None,
                status: ProjectReviewRunStatus::Running,
                outcome: None,
                pr: None,
                summary: None,
                error: None,
            },
            Vec::new(),
            Vec::new(),
        )
        .await?;
        let cycle_result = async {
            let message = self
                .project_reviewer_initial_message(project_id, reviewer_id)
                .await?;
            let turn_id = self
                .start_agent_turn_with_skills(
                    reviewer_id,
                    message,
                    vec!["reviewer-agent-review-pr".to_string()],
                )
                .await?;
            self.update_project_review_run_turn(project_id, run_id, reviewer_id, turn_id)
                .await?;
            let summary = self
                .wait_agent_until_complete_with_cancel(reviewer_id, &cancellation_token)
                .await?;
            if matches!(summary.status, AgentStatus::Failed | AgentStatus::Cancelled) {
                return Err(RuntimeError::InvalidInput(format!(
                    "reviewer ended with status {:?}",
                    summary.status
                )));
            }
            let response = self.last_turn_response(reviewer_id).await?.ok_or_else(|| {
                RuntimeError::InvalidInput("reviewer did not return a final response".to_string())
            })?;
            parse_project_review_cycle_report(&response)
        }
        .await;
        let turn_id = self
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
        let _ = self
            .finish_project_review_run(
                run_id,
                project_id,
                Some(reviewer_id),
                turn_id,
                status,
                outcome,
                pr,
                summary,
                error,
            )
            .await;
        let _ = self
            .cleanup_project_review_worktree(project_id, reviewer_id)
            .await;
        let _ = self.delete_agent(reviewer_id).await;
        self.set_project_review_state(
            project_id,
            ProjectReviewStatus::Idle,
            None,
            None,
            None,
            None,
            None,
            false,
        )
        .await?;
        cycle_result
    }

    async fn ensure_project_review_workspace(&self, project_id: ProjectId) -> Result<()> {
        self.run_project_review_repo_command(project_id, ReviewRepoCommand::Ensure)
            .await
    }

    async fn sync_project_review_repo(&self, project_id: ProjectId) -> Result<()> {
        self.run_project_review_repo_command(project_id, ReviewRepoCommand::Sync)
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
        let removed_runs = self.store.prune_project_review_runs_before(cutoff).await?;
        let removed_events = self.store.prune_service_events_before(cutoff).await?;
        if removed_runs > 0 || removed_events > 0 {
            tracing::info!(
                removed_runs,
                removed_events,
                "pruned project review history"
            );
        }
        self.recent_events
            .lock()
            .await
            .retain(|event| event.timestamp >= cutoff);
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
        let volume = DockerClient::workspace_volume_for_project_review(&project_id.to_string());
        let active_reviewer = match self.project(project_id).await {
            Ok(project) => project.summary.read().await.current_reviewer_agent_id,
            Err(_) => None,
        };
        let active_path = active_reviewer
            .map(|id| format!("/workspace/reviews/{id}"))
            .unwrap_or_default();
        let cutoff_epoch = cutoff.timestamp();
        let command = format!(
            "set -eu\n\
             git -C /workspace/repo worktree prune 2>/dev/null || true\n\
             mkdir -p /workspace/reviews\n\
             find /workspace/reviews -mindepth 1 -maxdepth 1 {active_filter} -type d ! -newermt @{cutoff_epoch} -exec rm -rf -- {{}} +\n\
             find /workspace/reviews -type f \\( -name '*.log' -o -name '*.tmp' -o -name '*.temp' -o -name 'tmp.*' \\) ! -newermt @{cutoff_epoch} -delete\n",
            active_filter = if active_path.is_empty() {
                String::new()
            } else {
                format!("! -path {}", shell_quote(&active_path))
            },
            cutoff_epoch = cutoff_epoch,
        );
        let output = self
            .docker
            .run_sidecar_shell_env(&mai_docker::SidecarParams {
                name: &format!("mai-review-retention-{project_id}"),
                image: &self.sidecar_image,
                command: &command,
                args: &[],
                cwd: Some("/workspace"),
                env: &[],
                workspace_volume: Some(&volume),
                timeout_secs: Some(120),
            })
            .await?;
        if output.status != 0 {
            return Err(RuntimeError::InvalidInput(format!(
                "review retention cleanup failed: {}",
                preview(format!("{}\n{}", output.stderr, output.stdout).trim(), 500)
            )));
        }
        Ok(())
    }

    async fn record_project_review_startup_failure(
        &self,
        project_id: ProjectId,
        error: String,
    ) -> Result<()> {
        let run_id = Uuid::new_v4();
        let started_at = now();
        self.save_project_review_run_status(
            ProjectReviewRunSummary {
                id: run_id,
                project_id,
                reviewer_agent_id: None,
                turn_id: None,
                started_at,
                finished_at: Some(now()),
                status: ProjectReviewRunStatus::Failed,
                outcome: Some(ProjectReviewOutcome::Failed),
                pr: None,
                summary: None,
                error: Some(error),
            },
            Vec::new(),
            Vec::new(),
        )
        .await
    }

    async fn cancel_active_project_review_runs(
        &self,
        project_id: ProjectId,
        reviewer_agent_id: Option<AgentId>,
    ) -> Result<()> {
        let runs = self
            .store
            .load_project_review_runs(project_id, None, 0, PROJECT_REVIEW_RUN_LIST_LIMIT)
            .await?;
        for run in runs {
            if run.finished_at.is_some()
                || !matches!(
                    run.status,
                    ProjectReviewRunStatus::Syncing | ProjectReviewRunStatus::Running
                )
                || reviewer_agent_id.is_some_and(|id| run.reviewer_agent_id != Some(id))
            {
                continue;
            }
            let _ = self
                .finish_project_review_run(
                    run.id,
                    project_id,
                    run.reviewer_agent_id,
                    run.turn_id,
                    ProjectReviewRunStatus::Cancelled,
                    None,
                    run.pr,
                    run.summary,
                    Some("review cancelled".to_string()),
                )
                .await;
        }
        Ok(())
    }

    async fn save_project_review_run_status(
        &self,
        summary: ProjectReviewRunSummary,
        messages: Vec<AgentMessage>,
        events: Vec<ServiceEvent>,
    ) -> Result<()> {
        self.store
            .save_project_review_run(&ProjectReviewRunDetail {
                summary,
                messages,
                events,
            })
            .await?;
        Ok(())
    }

    async fn update_project_review_run_turn(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
        reviewer_agent_id: AgentId,
        turn_id: TurnId,
    ) -> Result<()> {
        let Some(mut run) = self
            .store
            .load_project_review_run(project_id, run_id)
            .await?
        else {
            return Err(RuntimeError::ProjectReviewRunNotFound(run_id));
        };
        run.summary.reviewer_agent_id = Some(reviewer_agent_id);
        run.summary.turn_id = Some(turn_id);
        run.summary.status = ProjectReviewRunStatus::Running;
        self.store.save_project_review_run(&run).await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_project_review_run(
        &self,
        run_id: Uuid,
        project_id: ProjectId,
        reviewer_agent_id: Option<AgentId>,
        turn_id: Option<TurnId>,
        status: ProjectReviewRunStatus,
        outcome: Option<ProjectReviewOutcome>,
        pr: Option<u64>,
        summary_text: Option<String>,
        error: Option<String>,
    ) -> Result<()> {
        let Some(existing) = self
            .store
            .load_project_review_run(project_id, run_id)
            .await?
        else {
            return Err(RuntimeError::ProjectReviewRunNotFound(run_id));
        };
        let reviewer_agent_id = reviewer_agent_id.or(existing.summary.reviewer_agent_id);
        let turn_id = turn_id.or(existing.summary.turn_id);
        let (messages, events) = if let Some(reviewer_agent_id) = reviewer_agent_id {
            self.project_review_run_snapshot(reviewer_agent_id).await
        } else {
            (Vec::new(), Vec::new())
        };
        self.store
            .save_project_review_run(&ProjectReviewRunDetail {
                summary: ProjectReviewRunSummary {
                    id: run_id,
                    project_id,
                    reviewer_agent_id,
                    turn_id,
                    started_at: existing.summary.started_at,
                    finished_at: Some(now()),
                    status,
                    outcome,
                    pr,
                    summary: summary_text,
                    error,
                },
                messages,
                events,
            })
            .await?;
        Ok(())
    }

    async fn project_review_run_snapshot(
        &self,
        reviewer_agent_id: AgentId,
    ) -> (Vec<AgentMessage>, Vec<ServiceEvent>) {
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

    async fn cleanup_project_review_worktree(
        &self,
        project_id: ProjectId,
        reviewer_id: AgentId,
    ) -> Result<()> {
        let volume = DockerClient::workspace_volume_for_project_review(&project_id.to_string());
        let command = format!(
            "set -eu\n\
             git -C /workspace/repo worktree prune 2>/dev/null || true\n\
             rm -rf {}",
            shell_quote(&format!("/workspace/reviews/{reviewer_id}"))
        );
        let output = self
            .docker
            .run_sidecar_shell_env(&mai_docker::SidecarParams {
                name: &format!("mai-review-cleanup-{reviewer_id}"),
                image: &self.sidecar_image,
                command: &command,
                args: &[],
                cwd: Some("/workspace"),
                env: &[],
                workspace_volume: Some(&volume),
                timeout_secs: Some(120),
            })
            .await?;
        if output.status != 0 {
            return Err(RuntimeError::InvalidInput(format!(
                "review workspace cleanup failed: {}",
                preview(format!("{}\n{}", output.stderr, output.stdout).trim(), 500)
            )));
        }
        Ok(())
    }

    async fn run_project_review_repo_command(
        &self,
        project_id: ProjectId,
        command: ReviewRepoCommand,
    ) -> Result<()> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let token = self.project_git_token(project_id).await?.ok_or_else(|| {
            RuntimeError::InvalidInput("project git account token is not configured".to_string())
        })?;
        let volume = DockerClient::workspace_volume_for_project_review(&project_id.to_string());
        let repo_url = github_clone_url(&summary.owner, &summary.repo);
        let expected_remote = repo_url.clone();
        let branch = if summary.branch.trim().is_empty() {
            "main".to_string()
        } else {
            summary.branch.clone()
        };
        let (command_label, command_text) = match command {
            ReviewRepoCommand::Ensure => (
                "ensure",
                review_repo_ensure_command(&repo_url, &expected_remote, &branch),
            ),
            ReviewRepoCommand::Sync => (
                "sync",
                review_repo_sync_command(&repo_url, &expected_remote, &branch),
            ),
        };
        let env = [("MAI_GITHUB_REVIEW_TOKEN".to_string(), token.to_string())];
        let mut last_message = String::new();
        for attempt in 1..=PROJECT_REVIEW_REPO_COMMAND_MAX_ATTEMPTS {
            let sidecar_name = format!("mai-review-sync-{project_id}-{attempt}");
            let output = self
                .docker
                .run_sidecar_shell_env(&mai_docker::SidecarParams {
                    name: &sidecar_name,
                    image: &self.sidecar_image,
                    command: &command_text,
                    args: &[],
                    cwd: Some("/workspace"),
                    env: &env,
                    workspace_volume: Some(&volume),
                    timeout_secs: Some(900),
                })
                .await?;
            if output.status == 0 {
                return Ok(());
            }
            let combined = format!("{}\n{}", output.stderr, output.stdout);
            last_message = preview(redact_secret(combined.trim(), &token).trim(), 500);
            if attempt < PROJECT_REVIEW_REPO_COMMAND_MAX_ATTEMPTS {
                tracing::warn!(
                    project_id = %project_id,
                    command = command_label,
                    attempt,
                    attempts = PROJECT_REVIEW_REPO_COMMAND_MAX_ATTEMPTS,
                    "project review workspace command failed; retrying"
                );
                sleep(Duration::from_secs(PROJECT_REVIEW_REPO_COMMAND_RETRY_SECS)).await;
            }
        }
        Err(RuntimeError::InvalidInput(format!(
            "project review workspace sync failed after {} attempts: {last_message}",
            PROJECT_REVIEW_REPO_COMMAND_MAX_ATTEMPTS
        )))
    }

    async fn spawn_project_reviewer_agent(
        self: &Arc<Self>,
        project_id: ProjectId,
    ) -> Result<AgentSummary> {
        let project = self.project(project_id).await?;
        let project_summary = project.summary.read().await.clone();
        let maintainer = self.agent(project_summary.maintainer_agent_id).await?;
        let maintainer_summary = maintainer.summary.read().await.clone();
        let parent_container_id = self
            .ensure_agent_container(&maintainer, maintainer_summary.status.clone())
            .await?;
        let model = self.resolve_role_agent_model(AgentRole::Reviewer).await?;
        let workspace_volume =
            DockerClient::workspace_volume_for_project_review(&project_id.to_string());
        self.create_agent_with_container_source(
            CreateAgentRequest {
                name: Some(format!("{} Auto Reviewer", project_summary.name)),
                provider_id: Some(model.preference.provider_id),
                model: Some(model.preference.model),
                reasoning_effort: model.preference.reasoning_effort,
                docker_image: Some(maintainer_summary.docker_image.clone()),
                parent_id: Some(project_summary.maintainer_agent_id),
                system_prompt: Some(project_reviewer_system_prompt().to_string()),
            },
            ContainerSource::CloneFrom {
                parent_container_id,
                docker_image: maintainer_summary.docker_image,
                workspace_volume: Some(workspace_volume),
            },
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
    ) -> Result<String> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let extra = summary
            .reviewer_extra_prompt
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("None");
        Ok(format!(
            "Run one automatic pull request review for project `{}`.\n\nRepository: {}/{}\nDefault branch: {}\nWorkspace repo: /workspace/repo\nReview worktree root: /workspace/reviews/{}\n\nExtra reviewer instructions:\n{}\n\nUse the $reviewer-agent-review-pr skill. At the end of the turn, return only one JSON object matching this schema exactly:\n{{\"outcome\":\"review_submitted|no_eligible_pr|failed\",\"pr\":123|null,\"summary\":\"short result\",\"error\":null|\"failure reason\"}}",
            summary.name, summary.owner, summary.repo, summary.branch, reviewer_id, extra
        ))
    }

    async fn last_turn_response(&self, agent_id: AgentId) -> Result<Option<String>> {
        let agent = self.agent(agent_id).await?;
        let sessions = agent.sessions.lock().await;
        Ok(sessions
            .iter()
            .filter_map(|session| session.last_turn_response.clone())
            .last())
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
        current_reviewer_agent_id: Option<AgentId>,
        next_review_at: Option<DateTime<Utc>>,
        outcome: Option<ProjectReviewOutcome>,
        _summary_text: Option<String>,
        error: Option<String>,
        force_disabled: bool,
    ) -> Result<ProjectSummary> {
        let project = self.project(project_id).await?;
        let updated = {
            let mut summary = project.summary.write().await;
            summary.review_status = status;
            summary.current_reviewer_agent_id = current_reviewer_agent_id;
            summary.next_review_at = next_review_at;
            if current_reviewer_agent_id.is_some() {
                summary.last_review_started_at = Some(now());
                summary.last_review_finished_at = None;
            } else if outcome.is_some() || error.is_some() {
                summary.last_review_finished_at = Some(now());
            }
            if let Some(outcome) = outcome {
                summary.last_review_outcome = Some(outcome);
            }
            summary.review_last_error = error;
            if force_disabled {
                summary.auto_review_enabled = false;
            }
            summary.updated_at = now();
            summary.clone()
        };
        self.store.save_project(&updated).await?;
        self.publish(ServiceEventKind::ProjectUpdated {
            project: updated.clone(),
        })
        .await;
        Ok(updated)
    }

    async fn delete_project_review_workspace(&self, project_id: ProjectId) -> Result<()> {
        let volume = DockerClient::workspace_volume_for_project_review(&project_id.to_string());
        self.docker.delete_volume(&volume).await?;
        Ok(())
    }

    async fn prune_github_manifest_states(&self) {
        let ttl = Duration::from_secs(GITHUB_MANIFEST_STATE_TTL_SECS);
        let mut states = self.github_manifest_states.lock().await;
        states.retain(|_, state| state.created_at.elapsed() < ttl);
    }

    async fn take_github_manifest_state(&self, state: &str) -> Result<GithubManifestState> {
        self.prune_github_manifest_states().await;
        let record = self
            .github_manifest_states
            .lock()
            .await
            .remove(state)
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "GitHub App setup link expired or state is invalid. Start configuration again."
                        .to_string(),
                )
            })?;
        Ok(record)
    }

    async fn github_app_secret(&self) -> Result<(String, String, String)> {
        self.store.github_app_secret().await?.ok_or_else(|| {
            RuntimeError::InvalidInput(
                "GitHub App ID and private key must be configured before using Projects"
                    .to_string(),
            )
        })
    }

    async fn verified_git_account_repository(
        &self,
        account_id: &str,
        repository_full_name: &str,
    ) -> Result<VerifiedGithubRepository> {
        let token = self.git_account_token(account_id).await?;
        let repository_full_name = repository_full_name.trim();
        if !repository_full_name.contains('/') || repository_full_name.contains(char::is_whitespace)
        {
            return Err(RuntimeError::InvalidInput(
                "repository_full_name must look like owner/repo".to_string(),
            ));
        }
        let url = github_api_url(
            &self.github_api_base_url,
            &format!("/repos/{repository_full_name}"),
        );
        let response = self
            .github_http
            .get(url)
            .bearer_auth(&token)
            .headers(github_headers())
            .send()
            .await?;
        let repository: GithubRepositoryApi =
            decode_github_response(response, "get repository").await?;
        Ok(VerifiedGithubRepository {
            id: repository.id,
            owner: repository.owner.login,
            name: repository.name,
            full_name: repository.full_name,
            default_branch: repository
                .default_branch
                .unwrap_or_else(|| "main".to_string()),
        })
    }

    async fn git_account_summary(&self, account_id: &str) -> Result<GitAccountSummary> {
        self.store
            .list_git_accounts()
            .await?
            .accounts
            .into_iter()
            .find(|account| account.id == account_id)
            .ok_or_else(|| RuntimeError::InvalidInput("git account not found".to_string()))
    }

    async fn git_account_token(&self, account_id: &str) -> Result<String> {
        self.store
            .git_account_token(account_id)
            .await?
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| RuntimeError::InvalidInput("git account token not found".to_string()))
    }

    async fn github_app_jwt(&self) -> Result<(String, String)> {
        let (app_id, private_key, base_url) = self.github_app_secret().await?;
        let now = Utc::now().timestamp();
        let claims = GithubJwtClaims {
            iat: now.saturating_sub(60) as usize,
            exp: now.saturating_add(540) as usize,
            iss: app_id,
        };
        let token = encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &EncodingKey::from_rsa_pem(private_key.as_bytes())?,
        )?;
        Ok((token, base_url))
    }

    async fn github_installation_token(
        &self,
        installation_id: u64,
        repository_id: Option<u64>,
    ) -> Result<String> {
        let cache_key = format!(
            "{installation_id}:{}",
            repository_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "all".to_string())
        );
        {
            let tokens = self.github_tokens.lock().await;
            if let Some(cached) = tokens.get(&cache_key)
                && cached.expires_at - TimeDelta::seconds(GITHUB_TOKEN_REFRESH_SKEW_SECS)
                    > Utc::now()
            {
                return Ok(cached.token.clone());
            }
        }

        let (jwt, base_url) = self.github_app_jwt().await?;
        let url = github_api_url(
            &base_url,
            &format!("/app/installations/{installation_id}/access_tokens"),
        );
        let body = GithubAccessTokenRequest {
            repository_ids: repository_id.map(|id| vec![id]),
            permissions: GithubAccessTokenPermissions {
                contents: "write",
                pull_requests: "write",
                issues: "write",
            },
        };
        let response = self
            .github_http
            .post(url)
            .bearer_auth(jwt)
            .headers(github_headers())
            .json(&body)
            .send()
            .await?;
        let token: GithubAccessTokenResponse =
            decode_github_response(response, "create installation token").await?;
        self.github_tokens.lock().await.insert(
            cache_key,
            CachedGithubToken {
                token: token.token.clone(),
                expires_at: token.expires_at,
            },
        );
        Ok(token.token)
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
        let arguments = project_review_mcp_arguments_with_model_footer(
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
        Ok(ToolExecution {
            success: true,
            output: redact_secret(&output.to_string(), &token),
            ends_turn: false,
        })
    }

    async fn mcp_manager_for_tool(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> Result<Arc<McpAgentManager>> {
        if agent.summary.read().await.project_id.is_some() {
            return self
                .project_mcp_manager_for_agent(agent, agent_id, cancellation_token)
                .await?
                .ok_or_else(|| {
                    RuntimeError::InvalidInput("project MCP manager is not available".to_string())
                });
        }
        agent
            .mcp
            .read()
            .await
            .clone()
            .ok_or_else(|| RuntimeError::InvalidInput("MCP manager not initialized".to_string()))
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
        Ok(ToolExecution {
            success: status.is_success(),
            output: redact_secret(&output.to_string(), &token),
            ends_turn: false,
        })
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
        let token = self.git_account_token(&account_id).await?;
        let repo_url = github_clone_url(&summary.owner, &summary.repo);
        let sidecar = self.ensure_project_sidecar(project_id).await?;
        self.clone_repository_in_sidecar(&sidecar.id, &repo_url, summary.branch.trim(), &token)
            .await?;
        self.prepare_copied_project_workspace(&sidecar.id).await?;
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
        let mut history = {
            let sessions = agent.sessions.lock().await;
            sessions
                .iter()
                .find(|session| session.summary.id == session_id)
                .map(|session| session.history.clone())
                .ok_or(RuntimeError::SessionNotFound {
                    agent_id,
                    session_id,
                })?
        };
        if repair_incomplete_tool_history(&mut history) {
            self.replace_session_history(agent, agent_id, session_id, history.clone())
                .await?;
        }
        Ok(history)
    }

    async fn raw_session_history_len(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<usize> {
        let sessions = agent.sessions.lock().await;
        sessions
            .iter()
            .find(|session| session.summary.id == session_id)
            .map(|session| session.history.len())
            .ok_or(RuntimeError::SessionNotFound {
                agent_id,
                session_id,
            })
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
                workspace_volume,
            } => {
                if preferred_container_id.is_some() && workspace_volume.is_none() {
                    self.docker
                        .ensure_agent_container_from_image(
                            &agent_id.to_string(),
                            preferred_container_id.as_deref(),
                            docker_image,
                        )
                        .await
                } else {
                    self.docker
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
            self.publish(ServiceEventKind::McpServerStatusChanged {
                agent_id,
                server: server.clone(),
                status: mai_protocol::McpStartupStatus::Starting,
                error: None,
            })
            .await;
        }
        let mcp = McpAgentManager::start(self.docker.clone(), container.id, mcp_configs).await;
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
                .docker
                .delete_agent_containers(&agent_id.to_string(), Some(&container_id))
                .await;
            return Err(err);
        }
        for status in mcp.statuses().await {
            self.publish(ServiceEventKind::McpServerStatusChanged {
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
        if let Some(project_id) = agent.summary.read().await.project_id {
            let Some(manager) = self
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

fn required_any_string(arguments: &Value, fields: &[&str]) -> Result<String> {
    for field in fields {
        if let Some(value) = optional_string(arguments, field) {
            return Ok(value);
        }
    }
    Err(RuntimeError::InvalidInput(format!(
        "missing string field `{}`",
        fields.join("` or `")
    )))
}

fn optional_string(arguments: &Value, field: &str) -> Option<String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn collab_input_from_args(arguments: &Value) -> Result<CollabInput> {
    let mut input = CollabInput::default();
    if let Some(message) = optional_string(arguments, "message") {
        input.message = Some(message);
    }
    let Some(items) = arguments.get("items").and_then(Value::as_array) else {
        return Ok(input);
    };
    let mut parts = Vec::new();
    for item in items {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("text");
        match item_type {
            "text" => {
                let text = item.get("text").and_then(Value::as_str).ok_or_else(|| {
                    RuntimeError::InvalidInput("text collab items require `text`".to_string())
                })?;
                parts.push(text.to_string());
            }
            "skill" => {
                let mention = item
                    .get("path")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("name").and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput(
                            "skill collab items require `name` or `path`".to_string(),
                        )
                    })?;
                input.skill_mentions.push(mention.to_string());
            }
            _ => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unsupported collab item type `{item_type}`; expected text or skill"
                )));
            }
        }
    }
    if !parts.is_empty() {
        input.message = Some(match input.message {
            Some(existing) if !existing.is_empty() => format!("{existing}\n{}", parts.join("\n")),
            _ => parts.join("\n"),
        });
    }
    Ok(input)
}

fn agent_type_role(value: &str) -> Option<AgentRole> {
    match value.trim().to_lowercase().as_str() {
        "explorer" => Some(AgentRole::Explorer),
        "worker" | "default" | "" => Some(AgentRole::Executor),
        _ => None,
    }
}

fn wait_targets(arguments: &Value) -> Result<Vec<AgentId>> {
    if let Some(targets) = arguments.get("targets").and_then(Value::as_array) {
        if targets.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "targets must be non-empty".to_string(),
            ));
        }
        return targets
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput("targets must contain strings".to_string())
                    })
                    .and_then(parse_agent_id)
            })
            .collect();
    }
    Ok(vec![parse_agent_id(&required_string(
        arguments, "agent_id",
    )?)?])
}

fn wait_timeout(arguments: &Value) -> Duration {
    if let Some(ms) = arguments.get("timeout_ms").and_then(Value::as_u64) {
        return Duration::from_millis(ms);
    }
    Duration::from_secs(
        arguments
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_WAIT_AGENT_OBSERVATION_SECS),
    )
}

fn last_activity_at(
    summary: &AgentSummary,
    recent_messages: &[AgentMessage],
    recent_events: &[ServiceEvent],
) -> DateTime<Utc> {
    let mut timestamp = summary.updated_at;
    if let Some(message) = recent_messages.last() {
        timestamp = timestamp.max(message.created_at);
    }
    if let Some(event) = recent_events.last() {
        timestamp = timestamp.max(event.timestamp);
    }
    timestamp
}

fn active_tool_snapshot(recent_events: &[ServiceEvent]) -> Option<Value> {
    let mut completed = HashSet::new();
    for event in recent_events.iter().rev() {
        match &event.kind {
            ServiceEventKind::ToolCompleted { call_id, .. } => {
                completed.insert(call_id.clone());
            }
            ServiceEventKind::ToolStarted {
                turn_id,
                call_id,
                tool_name,
                arguments_preview,
                arguments,
                ..
            } if !completed.contains(call_id) => {
                return Some(json!({
                    "turn_id": turn_id,
                    "call_id": call_id,
                    "tool_name": tool_name,
                    "arguments_preview": arguments_preview,
                    "arguments": arguments,
                    "started_at": event.timestamp,
                }));
            }
            _ => {}
        }
    }
    None
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

fn review_repo_auth_prelude() -> &'static str {
    "tmp=$(mktemp -d)\n\
     askpass=\"$tmp/askpass.sh\"\n\
     cleanup() { rm -rf \"$tmp\"; }\n\
     trap cleanup EXIT HUP INT TERM\n\
     cat >\"$askpass\" <<'EOF'\n\
#!/bin/sh\n\
case \"$1\" in\n\
  *Username*) printf '%s\\n' x-access-token ;;\n\
  *Password*) printf '%s\\n' \"$MAI_GITHUB_REVIEW_TOKEN\" ;;\n\
  *) printf '\\n' ;;\n\
esac\n\
EOF\n\
     chmod 700 \"$askpass\"\n\
     export GIT_TERMINAL_PROMPT=0\n\
     export GIT_ASKPASS=\"$askpass\"\n\
     git config --global --add safe.directory /workspace/repo 2>/dev/null || true"
}

fn review_repo_ensure_command(repo_url: &str, expected_remote: &str, branch: &str) -> String {
    let branch_arg = if branch.trim().is_empty() {
        String::new()
    } else {
        format!(" --branch {}", shell_quote(branch))
    };
    format!(
        "set -eu\n\
         {prelude}\n\
         mkdir -p /workspace\n\
         if [ -d /workspace/repo/.git ]; then\n\
           remote=$(git -C /workspace/repo remote get-url origin 2>/dev/null || true)\n\
           if [ \"$remote\" != {expected_remote} ]; then\n\
             rm -rf /workspace/repo\n\
           fi\n\
         fi\n\
         if [ ! -d /workspace/repo/.git ]; then\n\
           rm -rf /workspace/repo\n\
           {git_network} clone{branch_arg} -- {repo_url} /workspace/repo\n\
         fi",
        prelude = review_repo_auth_prelude(),
        expected_remote = shell_quote(expected_remote),
        git_network = review_repo_git_network_command_prefix(),
        repo_url = shell_quote(repo_url),
    )
}

fn review_repo_sync_command(repo_url: &str, expected_remote: &str, branch: &str) -> String {
    let origin_branch = format!("origin/{branch}");
    format!(
        "set -eu\n\
         {ensure}\n\
         cd /workspace/repo\n\
         git -c credential.helper= remote set-url origin {repo_url}\n\
         {git_network} fetch --prune origin\n\
         {git_network} fetch origin '+refs/pull/*/head:refs/remotes/origin/pr/*'\n\
         git checkout {branch}\n\
         git reset --hard {origin_branch}\n\
         git worktree prune\n\
         mkdir -p /workspace/reviews",
        ensure = review_repo_ensure_command(repo_url, expected_remote, branch),
        git_network = review_repo_git_network_command_prefix(),
        repo_url = shell_quote(repo_url),
        branch = shell_quote(branch),
        origin_branch = shell_quote(&origin_branch),
    )
}

fn review_repo_git_network_command_prefix() -> String {
    format!(
        "git -c credential.helper= -c http.lowSpeedLimit={} -c http.lowSpeedTime={}",
        PROJECT_REVIEW_GIT_LOW_SPEED_LIMIT, PROJECT_REVIEW_GIT_LOW_SPEED_TIME_SECS
    )
}

fn project_reviewer_system_prompt() -> &'static str {
    "You are an autonomous project pull request reviewer. Review exactly one eligible GitHub pull request for this project, using only the GitHub MCP tools visible in the current tool list for GitHub reads/writes and local shell commands for git/test work. The repository has already been cloned and synced at /workspace/repo. Use an isolated git worktree under /workspace/reviews for the selected PR and clean it up before finishing. Do not look for GitHub tokens in the environment or write credentials. Finish with only the required JSON object so the project scheduler can decide the next cycle."
}

fn project_review_loop_decision_for_result(
    result: ProjectReviewCycleResult,
) -> ProjectReviewLoopDecision {
    match result.outcome {
        ProjectReviewOutcome::ReviewSubmitted => ProjectReviewLoopDecision {
            delay: Duration::ZERO,
            status: ProjectReviewStatus::Idle,
            outcome: Some(ProjectReviewOutcome::ReviewSubmitted),
            summary: result.summary,
            error: None,
        },
        ProjectReviewOutcome::NoEligiblePr => ProjectReviewLoopDecision {
            delay: Duration::from_secs(PROJECT_REVIEW_IDLE_RETRY_SECS),
            status: ProjectReviewStatus::Waiting,
            outcome: Some(ProjectReviewOutcome::NoEligiblePr),
            summary: result.summary,
            error: None,
        },
        ProjectReviewOutcome::Failed => ProjectReviewLoopDecision {
            delay: Duration::from_secs(PROJECT_REVIEW_FAILURE_RETRY_SECS),
            status: ProjectReviewStatus::Failed,
            outcome: Some(ProjectReviewOutcome::Failed),
            summary: result.summary,
            error: result
                .error
                .or_else(|| Some("reviewer reported failure".to_string())),
        },
    }
}

fn project_review_loop_decision_for_error(error: String) -> ProjectReviewLoopDecision {
    ProjectReviewLoopDecision {
        delay: Duration::from_secs(PROJECT_REVIEW_FAILURE_RETRY_SECS),
        status: ProjectReviewStatus::Failed,
        outcome: Some(ProjectReviewOutcome::Failed),
        summary: None,
        error: Some(error),
    }
}

fn parse_project_review_cycle_report(text: &str) -> Result<ProjectReviewCycleResult> {
    let value = match serde_json::from_str::<ProjectReviewCycleReport>(text) {
        Ok(value) => value,
        Err(_) => {
            let json = extract_json_object(text).ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "invalid project review final JSON: missing json object".to_string(),
                )
            })?;
            serde_json::from_str::<ProjectReviewCycleReport>(json).map_err(|err| {
                RuntimeError::InvalidInput(format!("invalid project review final JSON: {err}"))
            })?
        }
    };
    let summary = value
        .summary
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let pr_summary = value.pr.map(|pr| format!("PR #{pr}"));
    Ok(ProjectReviewCycleResult {
        outcome: value.outcome,
        pr: value.pr,
        summary: summary.or(pr_summary),
        error: value
            .error
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
    })
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return Some(&text[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

fn github_clone_url(owner: &str, repo: &str) -> String {
    format!("https://github.com/{owner}/{repo}.git")
}

fn github_repository_summary(repository: GithubRepositoryApi) -> GithubRepositorySummary {
    GithubRepositorySummary {
        id: repository.id,
        owner: repository.owner.login,
        name: repository.name,
        full_name: repository.full_name,
        private: repository.private,
        clone_url: repository.clone_url,
        html_url: repository.html_url,
        default_branch: repository.default_branch,
    }
}

fn github_package_belongs_to_repo(package: &GithubPackageApi, repository_full_name: &str) -> bool {
    package.repository.as_ref().is_some_and(|repository| {
        repository
            .full_name
            .eq_ignore_ascii_case(repository_full_name)
    })
}

fn repository_package_summary(
    owner: &str,
    package: GithubPackageApi,
    versions: Vec<GithubPackageVersionApi>,
) -> Option<RepositoryPackageSummary> {
    let tag = preferred_container_tag(&versions)?;
    let image = format!("ghcr.io/{}/{}:{}", owner, package.name, tag);
    Some(RepositoryPackageSummary {
        name: package.name,
        image,
        tag,
        html_url: package.html_url,
    })
}

fn preferred_container_tag(versions: &[GithubPackageVersionApi]) -> Option<String> {
    let mut first_tag = None;
    for version in versions {
        for tag in &version.metadata.container.tags {
            let tag = tag.trim();
            if tag.is_empty() {
                continue;
            }
            if tag == "latest" {
                return Some(tag.to_string());
            }
            if first_tag.is_none() {
                first_tag = Some(tag.to_string());
            }
        }
    }
    first_tag
}

fn github_api_url(base_url: &str, path: &str) -> String {
    let base = base_url
        .trim()
        .trim_end_matches('/')
        .if_empty(DEFAULT_GITHUB_API_BASE_URL);
    format!("{base}{path}")
}

fn normalize_github_api_get_path(path: &str) -> Result<String> {
    let path = path.trim();
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('#')
        || path.contains(char::is_whitespace)
    {
        return Err(RuntimeError::InvalidInput(
            "github_api_get path must be a GitHub API path beginning with `/`".to_string(),
        ));
    }
    Ok(path.to_string())
}

fn project_review_mcp_arguments_with_model_footer(
    model_tool_name: &str,
    mut arguments: Value,
    role: Option<&AgentRole>,
    model_id: &str,
) -> Value {
    if !matches!(role, Some(AgentRole::Reviewer))
        || !is_github_pull_request_review_write_tool(model_tool_name)
    {
        return arguments;
    }
    let Some(body) = arguments.get("body").and_then(Value::as_str) else {
        return arguments;
    };
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return arguments;
    }
    let footer = format!("Powered by {model_id}");
    if body.contains(&footer) {
        return arguments;
    }
    let body = format!("{}\n\n{}", body.trim_end(), footer);
    if let Some(value) = arguments.get_mut("body") {
        *value = Value::String(body);
    }
    arguments
}

fn is_github_pull_request_review_write_tool(model_tool_name: &str) -> bool {
    model_tool_name == PROJECT_GITHUB_PULL_REQUEST_REVIEW_WRITE_TOOL
        || model_tool_name == PROJECT_GITHUB_CREATE_PULL_REQUEST_REVIEW_TOOL
}

fn github_path_segment(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn sanitize_origin(origin: &str) -> Result<String> {
    let origin = origin.trim().trim_end_matches('/');
    if origin.is_empty() {
        return Err(RuntimeError::InvalidInput("origin is required".to_string()));
    }
    if !(origin.starts_with("http://") || origin.starts_with("https://")) {
        return Err(RuntimeError::InvalidInput(
            "origin must start with http:// or https://".to_string(),
        ));
    }
    if origin.contains('#') || origin.contains('?') || origin.contains(char::is_whitespace) {
        return Err(RuntimeError::InvalidInput(
            "origin must be a plain browser origin".to_string(),
        ));
    }
    Ok(origin.to_string())
}

fn is_valid_github_slug(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.len() <= 100
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn is_valid_github_manifest_code(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn github_app_manifest(redirect_url: &str, setup_url: &str, webhook_url: &str) -> Value {
    json!({
        "name": format!("Mai Team {}", Uuid::new_v4().to_string().split('-').next().unwrap_or("project")),
        "url": "https://github.com",
        "redirect_url": redirect_url,
        "callback_urls": [redirect_url],
        "setup_url": setup_url,
        "public": false,
        "default_permissions": {
            "contents": "write",
            "pull_requests": "write",
            "issues": "write"
        },
        "default_events": [],
        "hook_attributes": {
            "url": webhook_url,
            "active": false
        }
    })
}

fn github_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(USER_AGENT, HeaderValue::from_static("mai-team"));
    headers.insert(
        "X-GitHub-Api-Version",
        HeaderValue::from_static(GITHUB_API_VERSION),
    );
    headers
}

fn github_scopes(headers: &HeaderMap) -> Vec<String> {
    headers
        .get("x-oauth-scopes")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn git_token_kind(token: &str, scopes: &[String]) -> GitTokenKind {
    if token.starts_with("github_pat_") {
        GitTokenKind::FineGrainedPat
    } else if token.starts_with("ghp_") || !scopes.is_empty() {
        GitTokenKind::Classic
    } else {
        GitTokenKind::Unknown
    }
}

fn project_mcp_configs(token: &str) -> std::collections::BTreeMap<String, McpServerConfig> {
    let mut configs = std::collections::BTreeMap::new();
    configs.insert(
        PROJECT_GITHUB_MCP_SERVER.to_string(),
        McpServerConfig {
            scope: McpServerScope::Project,
            transport: McpServerTransport::Stdio,
            command: Some("github-mcp-server".to_string()),
            args: vec!["stdio".to_string()],
            env: std::collections::BTreeMap::from([
                (
                    "GITHUB_PERSONAL_ACCESS_TOKEN".to_string(),
                    token.to_string(),
                ),
                (
                    "GITHUB_TOOLSETS".to_string(),
                    "context,repos,issues,pull_requests".to_string(),
                ),
            ]),
            enabled: true,
            startup_timeout_secs: Some(20),
            ..McpServerConfig::default()
        },
    );
    configs.insert(
        PROJECT_GIT_MCP_SERVER.to_string(),
        McpServerConfig {
            scope: McpServerScope::Project,
            transport: McpServerTransport::Stdio,
            command: Some("uvx".to_string()),
            args: vec![
                "mcp-server-git".to_string(),
                "--repository".to_string(),
                PROJECT_WORKSPACE_PATH.to_string(),
            ],
            env: std::collections::BTreeMap::new(),
            enabled: true,
            startup_timeout_secs: Some(20),
            ..McpServerConfig::default()
        },
    );
    configs
}

async fn decode_github_response<T: DeserializeOwned>(
    response: reqwest::Response,
    action: &str,
) -> Result<T> {
    let status = response.status();
    if status.is_success() {
        return Ok(response.json::<T>().await?);
    }
    let text = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<GithubErrorResponse>(&text)
        .ok()
        .and_then(|error| error.message)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| preview(&text, 300));
    Err(RuntimeError::InvalidInput(format!(
        "GitHub {action} failed ({status}): {message}"
    )))
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

trait IfEmpty {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str;
}

impl IfEmpty for str {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str {
        if self.is_empty() { fallback } else { self }
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
            "You are the Executor for an approved task plan. Implement the requested changes in your container, keep scope tight, run verification, and report changed files plus test results. If reviewer feedback arrives, fix the issues and rerun relevant checks.\n\nWhen you have produced deliverable files (reports, generated code, data exports, documents, etc.), use the `save_artifact` tool to register each file so the user can download it. Always call `save_artifact` for any final output the user would want to keep."
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
        summary.status = AgentStatus::Idle;
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
    mai_skills::extract_skill_mentions(text)
}

fn skill_activation_info(skill_injections: &SkillInjections) -> Vec<SkillActivationInfo> {
    skill_injections
        .items
        .iter()
        .map(|skill| SkillActivationInfo {
            name: skill.metadata.name.clone(),
            display_name: skill
                .metadata
                .interface
                .as_ref()
                .and_then(|interface| interface.display_name.clone()),
            path: skill.metadata.path.clone(),
            scope: skill.metadata.scope,
        })
        .collect()
}

fn skill_user_fragment(skill_injections: &SkillInjections) -> Option<ModelInputItem> {
    if skill_injections.items.is_empty() {
        return None;
    }
    let mut text = String::new();
    for skill in &skill_injections.items {
        text.push_str(&format!(
            "\n<skill>\n<name>{}</name>\n<path>{}</path>\n{}\n</skill>\n",
            skill.metadata.name,
            skill.metadata.path.display(),
            skill.contents
        ));
    }
    Some(ModelInputItem::user_text(text))
}

impl ProjectSkillSourceDir {
    fn from_line(line: &str) -> Option<Self> {
        let mut parts = line.splitn(3, '\t');
        let relative = parts.next()?.trim();
        let cache_name = parts.next()?.trim();
        let container_path = parts.next()?.trim();
        if relative.is_empty() || cache_name.is_empty() || container_path.is_empty() {
            return None;
        }
        Some(Self {
            relative: PathBuf::from(relative),
            cache_name: cache_name.to_string(),
            container_path: container_path.to_string(),
        })
    }
}

fn normalize_copied_project_skill_dir(target: &Path, cache_name: &str) -> Result<()> {
    let nested = target.join(cache_name);
    if nested.is_dir() {
        let temp = target.with_extension("tmp");
        if temp.exists() {
            fs::remove_dir_all(&temp)?;
        }
        fs::rename(&nested, &temp)?;
        fs::remove_dir_all(target)?;
        fs::rename(temp, target)?;
    }
    Ok(())
}

fn project_skill_source_path(cache_dir: &Path, path: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(cache_dir).ok()?;
    let mut components = relative.components();
    let cache_name = match components.next()? {
        std::path::Component::Normal(name) => name.to_string_lossy(),
        _ => return None,
    };
    let source_relative = PROJECT_SKILL_CANDIDATE_DIRS
        .iter()
        .find(|(_, name)| *name == cache_name.as_ref())
        .map(|(relative, _)| *relative)?;
    let mut source_path = PathBuf::from(PROJECT_WORKSPACE_PATH).join(source_relative);
    for component in components {
        match component {
            std::path::Component::Normal(part) => source_path.push(part),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(source_path)
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

fn repair_incomplete_tool_history(history: &mut Vec<ModelInputItem>) -> bool {
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
    let changed = !insertions.is_empty();
    for (pos, item) in insertions.into_iter().rev() {
        history.insert(pos, item);
    }
    changed
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
        | ServiceEventKind::SkillsActivated { agent_id, .. }
        | ServiceEventKind::TodoListUpdated { agent_id, .. }
        | ServiceEventKind::McpServerStatusChanged { agent_id, .. }
        | ServiceEventKind::UserInputRequested { agent_id, .. } => Some(*agent_id),
        ServiceEventKind::TaskCreated { .. }
        | ServiceEventKind::TaskUpdated { .. }
        | ServiceEventKind::TaskDeleted { .. }
        | ServiceEventKind::ProjectCreated { .. }
        | ServiceEventKind::ProjectUpdated { .. }
        | ServiceEventKind::ProjectDeleted { .. }
        | ServiceEventKind::PlanUpdated { .. } => None,
        ServiceEventKind::ArtifactCreated { artifact } => Some(artifact.agent_id),
        ServiceEventKind::Error { agent_id, .. } => *agent_id,
    }
}

const BASE_INSTRUCTIONS: &str = r#"You are Mai, a coding agent running inside a Docker-backed multi-agent service.

General rules:
- You execute all local work inside your own Docker container; do not assume access to a host workspace.
- Use `container_exec` for shell commands inside your container.
- Use `container_cp_upload` and `container_cp_download` for file transfer.
- Use `spawn_agent`, `send_input`, `wait_agent`, `list_agents`, `close_agent`, and `resume_agent` for multi-agent collaboration.
- Use `list_mcp_resources`, `list_mcp_resource_templates`, and `read_mcp_resource` to inspect MCP resources when available.
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
        GitProvider, ModelConfig, ModelReasoningConfig, ModelReasoningVariant, ProviderConfig,
        ProviderKind, ProvidersConfigRequest,
    };
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn project_reviewer_review_body_gets_model_footer() {
        let arguments = json!({
            "body": "Looks good after local validation.",
            "comments": [
                {
                    "path": "src/lib.rs",
                    "line": 12,
                    "side": "RIGHT",
                    "body": "Please cover this edge case."
                }
            ]
        });

        let updated = project_review_mcp_arguments_with_model_footer(
            PROJECT_GITHUB_PULL_REQUEST_REVIEW_WRITE_TOOL,
            arguments,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        );

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("Looks good after local validation.\n\nPowered by gpt-5.4")
        );
        assert_eq!(
            updated.pointer("/comments/0/body").and_then(Value::as_str),
            Some("Please cover this edge case.")
        );
    }

    #[test]
    fn project_reviewer_review_body_footer_is_not_duplicated() {
        let arguments = json!({
            "body": "Looks good.\n\nPowered by gpt-5.4"
        });

        let updated = project_review_mcp_arguments_with_model_footer(
            PROJECT_GITHUB_PULL_REQUEST_REVIEW_WRITE_TOOL,
            arguments,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        );

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("Looks good.\n\nPowered by gpt-5.4")
        );
    }

    #[test]
    fn project_reviewer_review_body_footer_supports_legacy_tool_name() {
        let arguments = json!({
            "body": "Review submitted."
        });

        let updated = project_review_mcp_arguments_with_model_footer(
            PROJECT_GITHUB_CREATE_PULL_REQUEST_REVIEW_TOOL,
            arguments,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        );

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("Review submitted.\n\nPowered by gpt-5.4")
        );
    }

    #[test]
    fn project_reviewer_model_footer_leaves_non_review_tools_unchanged() {
        let arguments = json!({
            "body": "A regular issue comment."
        });

        let updated = project_review_mcp_arguments_with_model_footer(
            "mcp__github__add_issue_comment",
            arguments,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        );

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("A regular issue comment.")
        );
    }

    #[test]
    fn project_reviewer_model_footer_leaves_non_reviewer_agents_unchanged() {
        let arguments = json!({
            "body": "Maintainer review body."
        });

        let updated = project_review_mcp_arguments_with_model_footer(
            PROJECT_GITHUB_PULL_REQUEST_REVIEW_WRITE_TOOL,
            arguments,
            Some(&AgentRole::Planner),
            "gpt-5.4",
        );

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("Maintainer review body.")
        );
    }

    #[test]
    fn project_reviewer_model_footer_leaves_missing_or_non_string_body_unchanged() {
        let missing = json!({
            "event": "APPROVE"
        });
        let updated_missing = project_review_mcp_arguments_with_model_footer(
            PROJECT_GITHUB_PULL_REQUEST_REVIEW_WRITE_TOOL,
            missing,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        );
        assert_eq!(updated_missing, json!({ "event": "APPROVE" }));

        let non_string = json!({
            "body": null
        });
        let updated_non_string = project_review_mcp_arguments_with_model_footer(
            PROJECT_GITHUB_PULL_REQUEST_REVIEW_WRITE_TOOL,
            non_string,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        );
        assert_eq!(updated_non_string, json!({ "body": null }));
    }

    #[test]
    fn project_review_submitted_continues_immediately() {
        let decision = project_review_loop_decision_for_result(ProjectReviewCycleResult {
            outcome: ProjectReviewOutcome::ReviewSubmitted,
            pr: Some(123),
            summary: Some("submitted".to_string()),
            error: None,
        });

        assert_eq!(decision.delay, Duration::ZERO);
        assert_eq!(decision.status, ProjectReviewStatus::Idle);
        assert_eq!(
            decision.outcome,
            Some(ProjectReviewOutcome::ReviewSubmitted)
        );
        assert_eq!(decision.summary.as_deref(), Some("submitted"));
        assert_eq!(decision.error, None);
    }

    #[test]
    fn project_review_no_eligible_pr_waits_two_minutes() {
        let decision = project_review_loop_decision_for_result(ProjectReviewCycleResult {
            outcome: ProjectReviewOutcome::NoEligiblePr,
            pr: None,
            summary: Some("nothing to review".to_string()),
            error: None,
        });

        assert_eq!(
            decision.delay,
            Duration::from_secs(PROJECT_REVIEW_IDLE_RETRY_SECS)
        );
        assert_eq!(PROJECT_REVIEW_IDLE_RETRY_SECS, 120);
        assert_eq!(decision.status, ProjectReviewStatus::Waiting);
        assert_eq!(decision.outcome, Some(ProjectReviewOutcome::NoEligiblePr));
        assert_eq!(decision.summary.as_deref(), Some("nothing to review"));
        assert_eq!(decision.error, None);
    }

    #[test]
    fn project_review_failure_keeps_backoff() {
        let decision = project_review_loop_decision_for_result(ProjectReviewCycleResult {
            outcome: ProjectReviewOutcome::Failed,
            pr: None,
            summary: Some("failed".to_string()),
            error: None,
        });

        assert_eq!(
            decision.delay,
            Duration::from_secs(PROJECT_REVIEW_FAILURE_RETRY_SECS)
        );
        assert_eq!(decision.status, ProjectReviewStatus::Failed);
        assert_eq!(decision.outcome, Some(ProjectReviewOutcome::Failed));
        assert_eq!(decision.summary.as_deref(), Some("failed"));
        assert_eq!(decision.error.as_deref(), Some("reviewer reported failure"));
    }

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
                    write_mock_response(&mut stream, response).await;
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
        serde_json::from_slice(&buffer[header_end..header_end + content_length])
            .expect("request json")
    }

    async fn write_mock_response(stream: &mut tokio::net::TcpStream, response: Value) {
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
        let body = serde_json::to_string(&body_value).expect("response json");
        let reason = if status == 200 { "OK" } else { "ERROR" };
        let extra_headers = headers
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(name, value)| value.as_str().map(|value| format!("{name}: {value}\r\n")))
            .collect::<String>();
        let reply = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{}",
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

    async fn test_runtime(dir: &tempfile::TempDir, store: Arc<ConfigStore>) -> Arc<AgentRuntime> {
        AgentRuntime::new(
            DockerClient::new_with_binary("unused", fake_docker_path(dir)),
            ResponsesClient::new(),
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
            ResponsesClient::new(),
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
            ResponsesClient::new(),
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
	    exit 0
	    ;;
  cp)
    echo "$*" >> "$LOG"
    if [ "$2" = "created-container:/workspace/repo/.claude/skills" ]; then
      rm -rf "$3"
      cp -R "$WORKSPACE/.claude/skills" "$3"
    elif [ "$2" = "created-container:/workspace/repo/.agents/skills" ]; then
      rm -rf "$3"
      cp -R "$WORKSPACE/.agents/skills" "$3"
    elif [ "$2" = "created-container:/workspace/repo/skills" ]; then
      rm -rf "$3"
      cp -R "$WORKSPACE/skills" "$3"
    elif printf '%s' "$2" | grep -q '^created-container:'; then
      mkdir -p "$(dirname "$3")"
      printf 'artifact\n' > "$3"
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
    fn parses_project_review_cycle_json_from_final_text() {
        let report = parse_project_review_cycle_report(
            r#"Done.
            {"outcome":"no_eligible_pr","pr":null,"summary":"Nothing to review.","error":null}"#,
        )
        .expect("parse report");
        assert_eq!(report.outcome, ProjectReviewOutcome::NoEligiblePr);
        assert_eq!(report.summary.as_deref(), Some("Nothing to review."));
        assert_eq!(report.error, None);
    }

    #[test]
    fn project_review_sync_command_fetches_pr_refs_without_token_literal() {
        let command = review_repo_sync_command(
            "https://github.com/owner/repo.git",
            "https://github.com/owner/repo.git",
            "main",
        );
        assert!(command.contains("'+refs/pull/*/head:refs/remotes/origin/pr/*'"));
        assert!(command.contains("-c http.lowSpeedLimit=1 -c http.lowSpeedTime=30"));
        assert!(command.contains("git reset --hard origin/main"));
        assert!(command.contains("MAI_GITHUB_REVIEW_TOKEN"));
        assert!(!command.contains("ghp_"));
    }

    #[test]
    fn skill_user_fragment_wraps_loaded_skill_contents() {
        let path = std::path::PathBuf::from("/tmp/demo/SKILL.md");
        let fragment = skill_user_fragment(&SkillInjections {
            items: vec![mai_skills::LoadedSkill {
                metadata: mai_protocol::SkillMetadata {
                    name: "demo".to_string(),
                    description: "Demo skill".to_string(),
                    short_description: None,
                    path: path.clone(),
                    source_path: None,
                    scope: mai_protocol::SkillScope::Repo,
                    enabled: true,
                    interface: None,
                    dependencies: None,
                    policy: None,
                },
                contents: "skill body".to_string(),
            }],
            suggestions: Vec::new(),
            warnings: Vec::new(),
        })
        .expect("fragment");
        assert!(matches!(
            fragment,
            ModelInputItem::Message { role, content }
                if role == "user"
                    && matches!(&content[0], ModelContentItem::InputText { text }
                        if text.contains("<skill>")
                            && text.contains("<name>demo</name>")
                            && text.contains(path.to_string_lossy().as_ref())
                            && text.contains("skill body"))
        ));
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
        let mut history = vec![ModelInputItem::FunctionCall {
            call_id: "call_a".to_string(),
            name: "container_exec".to_string(),
            arguments: "{}".to_string(),
        }];
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
            ResponsesClient::new(),
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
            ResponsesClient::new(),
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
            ResponsesClient::new(),
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
            .save_session_context_tokens(agent_id, session_id, 80)
            .await
            .expect("save tokens");
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
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
            ResponsesClient::new(),
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
        let events = runtime.recent_events.lock().await;
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
    async fn user_turn_fuzzy_message_injects_skill() {
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
        let input = requests[0]["input"].as_array().expect("input");
        assert!(input.iter().any(|item| {
            item["role"] == "user"
                && item["content"][0]["text"].as_str().is_some_and(|text| {
                    text.contains("<name>frontend-app-builder</name>")
                        && text.contains("Use the frontend app builder flow.")
                })
        }));
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
                .recent_events
                .lock()
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
        assert!(
            runtime
                .recent_events
                .lock()
                .await
                .iter()
                .any(|event| matches!(
                    event.kind,
                    ServiceEventKind::TurnCompleted {
                        agent_id: event_agent_id,
                        turn_id: event_turn_id,
                        status: TurnStatus::Cancelled,
                        ..
                    } if event_agent_id == agent_id && event_turn_id == turn_id
                ))
        );
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
            .recent_events
            .lock()
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

        let completed = runtime
            .complete_turn_if_current(
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
        assert!(runtime.recent_events.lock().await.iter().all(|event| {
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
        let events = runtime.recent_events.lock().await;
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
            ResponsesClient::new(),
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
            ResponsesClient::new(),
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
            ResponsesClient::new(),
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
            ResponsesClient::new(),
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
    async fn project_detail_selects_live_reviewer_without_replacing_maintainer() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let project_id = Uuid::new_v4();
        let maintainer_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
        maintainer.project_id = Some(project_id);
        maintainer.role = Some(AgentRole::Planner);
        let mut reviewer = test_agent_summary_with_parent(
            reviewer_id,
            Some(maintainer_id),
            Some("reviewer-container"),
        );
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

        let visible = runtime.visible_tool_names(&worker_record, &[]).await;
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

        let visible = runtime.visible_tool_names(&maintainer_record, &[]).await;
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
        let visible = runtime.visible_tool_names(&maintainer_record, &tools).await;
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
        runtime.project_mcp_managers.write().await.insert(
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
        let visible = runtime.visible_tool_names(&maintainer_record, &tools).await;
        assert!(visible.contains("mcp__github__pull_request_review_write"));
        assert!(visible.contains("mcp__git__git_diff_unstaged"));
        assert!(!visible.contains("mcp__github__create_pull_request_review"));
        assert!(!visible.contains("mcp__git__git_status"));
    }

    #[test]
    fn project_mcp_configs_use_official_defaults_without_git_token_env() {
        let configs = project_mcp_configs("secret-token");
        let github = configs.get(PROJECT_GITHUB_MCP_SERVER).expect("github");
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
        let git = configs.get(PROJECT_GIT_MCP_SERVER).expect("git");
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
        runtime
            .start_next_queued_input(child_id)
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
            ResponsesClient::new(),
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
    fn repository_package_summary_prefers_latest_tag() {
        let package = GithubPackageApi {
            name: "mai-team-agent".to_string(),
            html_url: "https://github.com/orgs/example/packages/container/mai-team-agent"
                .to_string(),
            repository: Some(GithubPackageRepositoryApi {
                full_name: "example/mai-team".to_string(),
            }),
        };
        let versions = vec![
            GithubPackageVersionApi {
                metadata: GithubPackageVersionMetadataApi {
                    container: GithubPackageContainerMetadataApi {
                        tags: vec!["v1.2.0".to_string()],
                    },
                },
            },
            GithubPackageVersionApi {
                metadata: GithubPackageVersionMetadataApi {
                    container: GithubPackageContainerMetadataApi {
                        tags: vec!["latest".to_string(), "sha-123".to_string()],
                    },
                },
            },
        ];

        let summary = repository_package_summary("example", package, versions).expect("summary");

        assert_eq!(summary.tag, "latest");
        assert_eq!(summary.image, "ghcr.io/example/mai-team-agent:latest");
    }

    #[test]
    fn repository_package_summary_uses_first_available_tag() {
        let package = GithubPackageApi {
            name: "mai-team-sidecar".to_string(),
            html_url: "https://github.com/orgs/example/packages/container/mai-team-sidecar"
                .to_string(),
            repository: Some(GithubPackageRepositoryApi {
                full_name: "example/mai-team".to_string(),
            }),
        };
        let versions = vec![GithubPackageVersionApi {
            metadata: GithubPackageVersionMetadataApi {
                container: GithubPackageContainerMetadataApi {
                    tags: vec!["v1.2.0".to_string(), "sha-456".to_string()],
                },
            },
        }];

        let summary = repository_package_summary("example", package, versions).expect("summary");

        assert_eq!(summary.tag, "v1.2.0");
        assert_eq!(summary.image, "ghcr.io/example/mai-team-sidecar:v1.2.0");
    }

    #[test]
    fn github_package_match_requires_exact_repository() {
        let package = GithubPackageApi {
            name: "mai-team-agent".to_string(),
            html_url: "https://github.com/orgs/example/packages/container/mai-team-agent"
                .to_string(),
            repository: Some(GithubPackageRepositoryApi {
                full_name: "example/mai-team".to_string(),
            }),
        };
        let missing_repo_package = GithubPackageApi {
            name: "orphan-image".to_string(),
            html_url: "https://github.com/orgs/example/packages/container/orphan-image".to_string(),
            repository: None,
        };

        assert!(github_package_belongs_to_repo(&package, "example/mai-team"));
        assert!(!github_package_belongs_to_repo(&package, "example/other"));
        assert!(!github_package_belongs_to_repo(
            &missing_repo_package,
            "example/mai-team"
        ));
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
            .project_reviewer_initial_message(project_id, reviewer_id)
            .await
            .expect("message");

        assert!(message.contains("new prompt"));
        assert!(!message.contains("old prompt"));
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
            .publish(ServiceEventKind::TurnCompleted {
                agent_id: reviewer_id,
                session_id: Some(session_id),
                turn_id,
                status: TurnStatus::Completed,
            })
            .await;

        runtime
            .finish_project_review_run(
                run_id,
                project_id,
                Some(reviewer_id),
                Some(turn_id),
                ProjectReviewRunStatus::Completed,
                Some(ProjectReviewOutcome::ReviewSubmitted),
                Some(12),
                Some("submitted".to_string()),
                None,
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
        runtime.project_mcp_managers.write().await.insert(
            project_id,
            Arc::new(McpAgentManager::from_tools_for_test(vec![test_mcp_tool(
                "github", "get_me",
            )])),
        );

        runtime.delete_project(project_id).await.expect("delete");

        assert!(
            !runtime
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
            ResponsesClient::new(),
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
            ResponsesClient::new(),
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
            ResponsesClient::new(),
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
            ResponsesClient::new(),
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

    #[tokio::test]
    async fn github_manifest_start_builds_org_action_and_manifest() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let runtime = test_runtime(&dir, store).await;

        let response = runtime
            .start_github_app_manifest(GithubAppManifestStartRequest {
                origin: "http://127.0.0.1:8080/".to_string(),
                account_type: GithubAppManifestAccountType::Organization,
                org: Some("mai-org".to_string()),
            })
            .await
            .expect("start manifest");

        assert!(
            response
                .action_url
                .starts_with("https://github.com/organizations/mai-org/settings/apps/new?state=")
        );
        assert!(response.action_url.ends_with(&response.state));
        assert_eq!(
            response.manifest["redirect_url"],
            "http://127.0.0.1:8080/github/app-manifest/callback"
        );
        assert_eq!(
            response.manifest["default_permissions"]["contents"],
            "write"
        );
        assert_eq!(
            response.manifest["default_permissions"]["pull_requests"],
            "write"
        );
        assert_eq!(response.manifest["default_permissions"]["issues"], "write");
        assert_eq!(response.manifest["hook_attributes"]["active"], false);
    }

    #[tokio::test]
    async fn github_manifest_start_rejects_invalid_org() {
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        let runtime = test_runtime(&dir, store).await;

        let result = runtime
            .start_github_app_manifest(GithubAppManifestStartRequest {
                origin: "http://127.0.0.1:8080".to_string(),
                account_type: GithubAppManifestAccountType::Organization,
                org: Some("-bad-".to_string()),
            })
            .await;

        assert!(matches!(result, Err(RuntimeError::InvalidInput(_))));
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
