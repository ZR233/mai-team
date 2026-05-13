pub(crate) use chrono::{DateTime, Utc};
pub(crate) use mai_protocol::{
    AgentConfigRequest, AgentId, AgentLogEntry, AgentMessage, AgentSessionSummary, AgentSummary,
    ArtifactInfo, GitAccountRequest, GitAccountStatus, GitAccountSummary, GitAccountsResponse,
    GitProvider, GitTokenKind, GithubAppSettingsRequest, GithubAppSettingsResponse,
    GithubSettingsResponse, McpServerConfig, ModelCapabilities, ModelConfig, ModelInputItem,
    ModelReasoningConfig, ModelReasoningVariant, ModelRequestPolicy, ModelWireApi,
    PlanHistoryEntry, ProjectId, ProjectReviewRunDetail, ProjectReviewRunSummary, ProjectSummary,
    ProviderConfig, ProviderKind, ProviderPreset, ProviderPresetsResponse, ProviderSecret,
    ProviderSummary, ProvidersConfigRequest, ProvidersResponse, ServiceEvent, ServiceEventKind,
    SessionId, SkillsConfigRequest, TaskId, TaskPlan, TaskReview, TaskSummary, TokenUsage,
    ToolOutputArtifactInfo, ToolTraceDetail, ToolTraceSummary, TurnId, default_true,
};
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use std::collections::{BTreeMap, BTreeSet};
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::str::FromStr;
pub(crate) use std::time::SystemTime;
pub(crate) use toasty::Db;
pub(crate) use toasty::stmt::{List, Query};
pub(crate) use uuid::Uuid;

use thiserror::Error;

mod artifacts;
mod convert;
mod events;
mod git_accounts;
mod github_app;
mod logs;
mod projects;
mod providers;
mod records;
mod runtime_state;
mod schema;
mod settings;
mod store;
mod tasks;

#[cfg(test)]
mod tests;

pub use providers::ProviderSelection;
pub use store::ConfigStore;

pub(crate) use convert::*;

const SETTING_AGENT_CONFIG: &str = "agent_config";
const SETTING_SKILLS_CONFIG: &str = "skills_config";
const SETTING_GITHUB_TOKEN: &str = "github_token";
const SETTING_GITHUB_APP_CONFIG: &str = "github_app_config";
const SETTING_GIT_ACCOUNTS: &str = "git_accounts";
const DEFAULT_GITHUB_API_BASE_URL: &str = "https://api.github.com";
const DEEPSEEK_V4_CONTEXT_TOKENS: u64 = 1_000_000;
const DEEPSEEK_V4_OUTPUT_TOKENS: u64 = 384_000;

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
    #[error("invalid config: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone)]
pub struct PersistedAgent {
    pub summary: AgentSummary,
    pub sessions: Vec<PersistedAgentSession>,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PersistedAgentSession {
    pub summary: AgentSessionSummary,
    pub history: Vec<ModelInputItem>,
    pub last_context_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RuntimeSnapshot {
    pub agents: Vec<PersistedAgent>,
    pub tasks: Vec<PersistedTask>,
    pub projects: Vec<ProjectSummary>,
    pub recent_events: Vec<ServiceEvent>,
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

#[derive(Debug, Clone, Default)]
pub struct AgentLogFilter {
    pub session_id: Option<SessionId>,
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
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub offset: usize,
    pub limit: usize,
}
