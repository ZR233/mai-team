use crate::agents::profiles::AgentProfilesManager;
use crate::mcp::McpAgentManager;
use crate::skills::{SkillInjections, SkillsManager};
use chrono::{DateTime, Utc};
use mai_docker::{
    ContainerHandle, DockerClient, SidecarParams, agent_workspace_volume,
    project_agent_workspace_volume, project_cache_volume,
};
use mai_protocol::*;
use mai_store::{AgentLogFilter, MaiStore, ToolTraceFilter};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{Mutex, OnceCell, RwLock, broadcast};
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod agent_host;
mod agents;
mod config;
mod deps;
mod events;
mod facade;
pub mod github;
mod instructions;
mod mcp;
mod model_profile;
mod model_projection;
mod projects;
mod runtime_agent_api;
mod runtime_agent_traits;
mod runtime_bootstrap;
mod runtime_config;
mod runtime_environment;
mod runtime_product_api;
mod runtime_project_traits;
mod runtime_provisioning;
mod runtime_resources;
mod runtime_review_traits;
mod runtime_skills;
mod runtime_task_traits;
mod runtime_workspace;
mod skills;
mod state;
mod tasks;
mod tools;
mod turn;

use agents::AgentResourceBroker;
pub use config::{
    MAI_CONFIG_SCHEMA_VERSION, MaiConfig, MaiContainerConfig, MaiGithubConfig,
    MaiInstructionsConfig, MaiMcpConfig, MaiReviewConfig, MaiSkillsConfig, model_config_from_api,
    provider_catalog_snapshot, seed_default_provider_from_env,
};
use deps::RuntimeDeps;
use events::{RECENT_EVENT_LIMIT, RuntimeEvents};
use github::{
    DEFAULT_GITHUB_API_BASE_URL, DirectGithubAppBackend, GITHUB_HTTP_TIMEOUT_SECS, GithubAppBackend,
};
use instructions::{CONTAINER_SKILLS_ROOT, ContainerSkillPaths};
pub use model_profile::{core_model_turn_request, core_provider_for_selection};
pub use model_projection::{completion_response_to_model_response, completion_response_usage};
use pl_core::{
    GIT_TOKEN_ENV, git_shell_credential_prelude, git_shell_retry_function, shell_quote_word,
};
use projects::instructions::ProjectInstructionSourceFile;
use projects::review::ProjectReviewCycleResult;
use projects::review::pool::{ProjectReviewPoolEnqueueSummary, ProjectReviewSignalInput};
use projects::review::relay_queue::{
    ProjectReviewRelayQueueEnqueueSummary, ProjectReviewRelaySignalInput,
};
use projects::review::runs::FinishReviewRun;
use projects::review::state::ReviewStateUpdate;
use projects::skills::ProjectSkillSourceDir;
use projects::workspace::ProjectWorkspaceManager;
use state::{AgentRecord, ProjectRecord, RuntimeState, TaskRecord};

const AUTO_COMPACT_THRESHOLD_PERCENT: u64 = 90;
const PROJECT_REVIEW_RUN_LIST_LIMIT: usize = 50;
const PROJECT_REVIEW_SNAPSHOT_MESSAGE_LIMIT: usize = 40;
const PROJECT_REVIEW_SNAPSHOT_EVENT_LIMIT: usize = 80;
const DEFAULT_SIDECAR_IMAGE: &str = "ghcr.io/zr233/mai-team-sidecar:latest";
const UNCONFIGURED_PROVIDER_ID: &str = "unconfigured";
const UNCONFIGURED_PROVIDER_NAME: &str = "No provider configured";
const UNCONFIGURED_MODEL_ID: &str = "unconfigured";
const PROJECT_CACHE_VOLUME_MISSING_AFTER_STARTUP_RECONCILE: &str =
    "project cache volume is missing after startup reconcile";
const PROJECT_AGENT_WORKSPACE_VOLUME_MISSING_AFTER_STARTUP_RECONCILE: &str =
    "project agent workspace volume is missing after startup reconcile";
const RELAY_ENABLED_BUT_NOT_CONNECTED: &str = "relay is enabled but not connected";
const RELAY_NOT_CONNECTED: &str = "relay is not connected";
const SQLITE_DATABASE_LOCKED: &str = "database is locked";
const COMPACT_USER_MESSAGE_MAX_CHARS: usize = 80_000;
const COMPACT_SUMMARY_PREFIX: &str = "Context checkpoint summary from earlier conversation history. This is background for continuity, not a new user request.";
const COMPACT_PROMPT: &str = r#"You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will continue this agent session.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done as clear next steps
- Any critical data, examples, file paths, command outputs, or references needed to continue

Be concise, structured, and focused on helping the next model seamlessly continue the work."#;
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

impl From<ProjectReviewRelayQueueEnqueueSummary> for ProjectReviewQueueSummary {
    fn from(value: ProjectReviewRelayQueueEnqueueSummary) -> Self {
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
    Model(#[from] pl_protocol::PureError),
    #[error("mcp error: {0}")]
    Mcp(#[from] crate::mcp::McpError),
    #[error("store error: {0}")]
    Store(#[from] mai_store::StoreError),
    #[error("skill error: {0}")]
    Skill(#[from] crate::skills::SkillError),
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

/// mai wire provider/model 的已解析组合；PL 类型只存在于 config conversion 边界内。
#[derive(Debug, Clone)]
pub struct ProviderSelection {
    pub provider: ProviderSecret,
    pub model: ModelConfig,
}

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
    mai_config: Arc<RwLock<MaiConfig>>,
    agent_framework: OnceCell<pl_core::AgentRuntime<agent_host::MaiAgentHost>>,
    cache_root: PathBuf,
    artifact_files_root: PathBuf,
    sidecar_image: String,
    github_api_base_url: String,
    workspace_manager: projects::workspace::LocalProjectWorkspaceManager,
}

struct ResolvedAgentModel {
    preference: AgentModelPreference,
    effective: ResolvedAgentModelPreference,
}

fn initial_framework_session(summary: &AgentSummary) -> pl_core::AgentSessionState {
    let task_owned = summary.task_id.is_some() && summary.role != Some(AgentRole::Planner);
    agent_host::session_state(
        pl_core::SessionId::generate(),
        if task_owned { "Task" } else { "Chat 1" }.to_string(),
    )
}

fn framework_depth(agent_id: AgentId, agents: &HashMap<AgentId, AgentSummary>) -> u32 {
    let mut current = agent_id;
    let mut depth = 0_u32;
    let mut remaining = agents.len();
    while remaining > 0 {
        let Some(parent_id) = agents.get(&current).and_then(|agent| agent.parent_id) else {
            break;
        };
        current = parent_id;
        depth = depth.saturating_add(1);
        remaining -= 1;
    }
    depth
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
        transport: selection.provider.transport,
        model: selection.model.id,
        model_name: selection.model.name,
        reasoning_effort,
        context_tokens: selection.model.context_tokens,
        max_context_tokens: selection.model.max_context_tokens,
        effective_context_window_percent: selection.model.effective_context_window_percent,
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

fn project_failed_by_recoverable_workspace_start_error(project: &ProjectSummary) -> bool {
    project.status == ProjectStatus::Failed
        && project.clone_status == ProjectCloneStatus::Failed
        && project
            .last_error
            .as_deref()
            .is_some_and(project_workspace_start_error_is_recoverable)
}

fn project_workspace_needs_startup_resume(project: &ProjectSummary) -> bool {
    project_failed_by_recoverable_workspace_start_error(project)
        || project_workspace_creation_was_interrupted(&project.status, &project.clone_status)
}

fn project_workspace_creation_was_interrupted(
    status: &ProjectStatus,
    clone_status: &ProjectCloneStatus,
) -> bool {
    match status {
        ProjectStatus::Creating => match clone_status {
            ProjectCloneStatus::Pending
            | ProjectCloneStatus::Cloning
            | ProjectCloneStatus::Ready => true,
            ProjectCloneStatus::Failed => false,
        },
        ProjectStatus::Ready | ProjectStatus::Failed | ProjectStatus::Deleting => false,
    }
}

fn project_workspace_start_error_is_recoverable(error: &str) -> bool {
    let error = error.trim();
    let error = error.strip_prefix("invalid input: ").unwrap_or(error);
    matches!(
        error,
        PROJECT_CACHE_VOLUME_MISSING_AFTER_STARTUP_RECONCILE
            | RELAY_ENABLED_BUT_NOT_CONNECTED
            | RELAY_NOT_CONNECTED
    ) || error.contains(SQLITE_DATABASE_LOCKED)
}

fn redact_secret(value: &str, secret: &str) -> String {
    pl_core::SecretRedaction::new([secret]).redact_str(value)
}

fn normalized_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn is_stale_agent_model_selection_error(error: &RuntimeError) -> bool {
    let RuntimeError::Store(mai_store::StoreError::InvalidConfig(message)) = error else {
        return false;
    };
    (message.starts_with("provider `") && message.ends_with("` not found"))
        || (message.starts_with("model `")
            && message.contains("` is not configured for provider `"))
}

fn runtime_sidecar_image(image: String) -> String {
    let image = image.trim();
    if image.is_empty() {
        DEFAULT_SIDECAR_IMAGE.to_string()
    } else {
        image.to_string()
    }
}

#[cfg(test)]
mod framework_tests;
