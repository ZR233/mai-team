pub(crate) use chrono::{DateTime, Utc};
pub(crate) use mai_protocol::{
    AgentId, AgentLogEntry, AgentSummary, ArtifactInfo, GitAccountRequest, GitAccountStatus,
    GitAccountSummary, GitAccountsResponse, GitProvider, GitTokenKind, GithubAppSettingsRequest,
    GithubAppSettingsResponse, GithubSettingsResponse, MaiProductEventEnvelope,
    MaiProductEventKind, McpServerConfig, PlanHistoryEntry, ProjectId,
    ProjectPullRequestLifecycleState, ProjectPullRequestReviewHistoryItem,
    ProjectPullRequestReviewHistoryPage, ProjectPullRequestReviewPage,
    ProjectPullRequestReviewStatusSummary, ProjectPullRequestReviewSummary,
    ProjectReviewJobSummary, ProjectReviewRunDetail, ProjectReviewRunSummary, ProjectSummary,
    RelaySettingsRequest, RelaySettingsResponse, SkillsConfigRequest, TaskId, TaskPlan, TaskReview,
    TaskSummary, ThreadId, TokenUsage, ToolOutputArtifactInfo, ToolTraceDetail, ToolTraceSummary,
    TurnId,
};
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use std::collections::BTreeMap;
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::str::FromStr;
pub(crate) use toasty::Db;
pub(crate) use toasty::stmt::{List, Query};
pub(crate) use uuid::Uuid;

use thiserror::Error;

mod artifacts;
mod cleanup_tasks;
mod config_document;
mod convert;
mod events;
mod git_accounts;
mod github_app;
pub use github_app::GithubAppIdentity;
mod logs;
mod project_pull_request_states;
mod projects;
mod records;
mod relay;
mod review_ci_watches;
mod review_jobs;
mod runtime_state;
mod schema;
mod settings;
mod sqlite_busy;
mod store;
mod tasks;
mod thread_runtime;

#[cfg(test)]
mod tests;

pub use cleanup_tasks::{
    ProjectReviewCleanupResourceKind, ProjectReviewCleanupTask, ProjectReviewCleanupTaskStatus,
};
pub use config_document::ConfigDocumentStore;
pub use review_ci_watches::ProjectReviewCiWatch;
pub use review_jobs::{
    ProjectReviewCiPendingSkipResult, ProjectReviewCiWatchEnqueueResult,
    ProjectReviewJobEnqueueDisposition, ProjectReviewJobEnqueueResult,
    ProjectReviewReviewableJobEnqueueResult, REVIEW_JOB_SQLITE_BUSY_TIMEOUT_SECS,
};
pub use sqlite_busy::is_retryable_sqlite_error;
pub use store::MaiStore;
pub use thread_runtime::{
    StoredThreadRuntime, StoredThreadRuntimeEvent, StoredThreadSubmission,
    StoredThreadSubmissionPage, StoredThreadTraceEvent, ThreadRuntimeCommitDocument,
    ThreadRuntimeCommitOutcome, ThreadRuntimeTurnCommit,
};

pub(crate) use convert::*;

const SETTING_SKILLS_CONFIG: &str = "skills_config";
const SETTING_GITHUB_TOKEN: &str = "github_token";
const SETTING_GITHUB_APP_CONFIG: &str = "github_app_config";
const SETTING_GIT_ACCOUNTS: &str = "git_accounts";
const SETTING_RELAY_CONFIG: &str = "relay_config";
const DEFAULT_GITHUB_API_BASE_URL: &str = "https://api.github.com";
const DEFAULT_RELAY_URL: &str = "http://127.0.0.1:8090";
const DEFAULT_RELAY_NODE_ID: &str = "mai-server";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("toasty error: {0}")]
    Toasty(#[from] toasty::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("toml error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("toml serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),
    #[error("time parse error: {0}")]
    Time(#[from] chrono::ParseError),
    #[error("parse error: {0}")]
    Parse(#[from] strum::ParseError),
    #[error("data integrity error: {0}")]
    DataIntegrity(String),
    #[error("invalid config: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone)]
pub struct PersistedAgent {
    pub summary: AgentSummary,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RuntimeSnapshot {
    pub agents: Vec<PersistedAgent>,
    pub tasks: Vec<PersistedTask>,
    pub projects: Vec<ProjectSummary>,
    pub recent_events: Vec<MaiProductEventEnvelope>,
    pub next_sequence: u64,
}

#[derive(Debug, Clone)]
pub struct PersistedTask {
    pub summary: TaskSummary,
    pub plan: TaskPlan,
    pub plan_history: Vec<PlanHistoryEntry>,
    pub reviews: Vec<TaskReview>,
    pub artifacts: Vec<ArtifactInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedPullRequestStateObservation {
    pub pr: u64,
    pub state: ProjectPullRequestLifecycleState,
    pub state_changed_at: Option<DateTime<Utc>>,
    pub detected_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PersistedPullRequestStateSaveSummary {
    pub newly_merged: usize,
    pub newly_closed: usize,
}

#[derive(Debug, Clone, Default)]
pub struct AgentLogFilter {
    pub thread_id: Option<ThreadId>,
    pub turn_id: Option<TurnId>,
    pub level: Option<String>,
    pub category: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ToolTraceFilter {
    pub thread_id: Option<ThreadId>,
    pub turn_id: Option<TurnId>,
    pub offset: usize,
    pub limit: usize,
}
