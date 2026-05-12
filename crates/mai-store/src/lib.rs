use chrono::{DateTime, Utc};
use mai_protocol::{
    AgentConfigRequest, AgentId, AgentLogEntry, AgentMessage, AgentSessionSummary, AgentSummary,
    ArtifactInfo, GitAccountRequest, GitAccountStatus, GitAccountSummary, GitAccountsResponse,
    GitProvider, GitTokenKind, GithubAppSettingsRequest, GithubAppSettingsResponse,
    GithubSettingsResponse, McpServerConfig, ModelCapabilities, ModelConfig, ModelInputItem,
    ModelReasoningConfig, ModelReasoningVariant, ModelRequestPolicy, ModelWireApi,
    PlanHistoryEntry, ProjectId, ProjectReviewRunDetail, ProjectReviewRunSummary, ProjectSummary,
    ProviderConfig, ProviderKind, ProviderPreset, ProviderPresetsResponse, ProviderSecret,
    ProviderSummary, ProvidersConfigRequest, ProvidersResponse, ServiceEvent, ServiceEventKind,
    SessionId, SkillsConfigRequest, TaskId, TaskPlan, TaskReview, TaskSummary, TokenUsage,
    ToolTraceDetail, ToolTraceSummary, TurnId, default_true,
};
use rusqlite::Connection as SqliteConnection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::SystemTime;
use thiserror::Error;
use toasty::Db;
use toasty::stmt::{List, Query};
use toasty_driver_sqlite::Sqlite;
use tokio::sync::Mutex;
use uuid::Uuid;

const SETTING_AGENT_CONFIG: &str = "agent_config";
const SETTING_SKILLS_CONFIG: &str = "skills_config";
const SETTING_GITHUB_TOKEN: &str = "github_token";
const SETTING_GITHUB_APP_CONFIG: &str = "github_app_config";
const SETTING_GIT_ACCOUNTS: &str = "git_accounts";
const SETTING_SCHEMA_VERSION: &str = "toasty_schema_version";
const SCHEMA_VERSION: &str = "16";
const DEFAULT_GITHUB_API_BASE_URL: &str = "https://api.github.com";
const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";
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

pub struct ConfigStore {
    path: PathBuf,
    config_path: PathBuf,
    artifact_index_dir: PathBuf,
    db: Db,
    git_accounts_lock: Mutex<()>,
    providers_cache: Mutex<Option<ProvidersCache>>,
}

#[derive(Debug, Clone)]
pub struct ProviderSelection {
    pub provider: ProviderSecret,
    pub model: ModelConfig,
}

#[derive(Debug, Clone)]
struct ProvidersCache {
    stamp: ProvidersCacheStamp,
    file: ProvidersToml,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProvidersCacheStamp {
    exists: bool,
    modified: Option<SystemTime>,
    len: u64,
}

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct GithubAppConfig {
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    private_key: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    app_slug: Option<String>,
    #[serde(default)]
    app_html_url: Option<String>,
    #[serde(default)]
    owner_login: Option<String>,
    #[serde(default)]
    owner_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct GitAccountsConfig {
    #[serde(default)]
    default_account_id: Option<String>,
    #[serde(default)]
    accounts: Vec<StoredGitAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredGitAccount {
    id: String,
    #[serde(default)]
    provider: GitProvider,
    label: String,
    #[serde(default)]
    login: Option<String>,
    #[serde(default)]
    token_kind: GitTokenKind,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    status: GitAccountStatus,
    #[serde(default)]
    is_default: bool,
    token_secret: String,
    #[serde(default)]
    last_verified_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_error: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ProvidersToml {
    #[serde(default)]
    default_provider_id: Option<String>,
    #[serde(default)]
    providers: BTreeMap<String, ProviderToml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderToml {
    kind: ProviderKind,
    name: String,
    base_url: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    default_model: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    models: BTreeMap<String, ModelToml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelToml {
    #[serde(default)]
    name: Option<String>,
    context_tokens: u64,
    output_tokens: u64,
    #[serde(default = "default_true")]
    supports_tools: bool,
    #[serde(default)]
    wire_api: ModelWireApi,
    #[serde(default)]
    capabilities: ModelCapabilities,
    #[serde(default)]
    request_policy: ModelRequestPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning: Option<ModelReasoningConfig>,
    #[serde(default, skip_serializing_if = "is_null")]
    options: serde_json::Value,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "mcp_servers"]
struct McpServerRecord {
    #[key]
    name: String,
    config_json: String,
    enabled: bool,
    sort_order: i64,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "settings"]
struct SettingRecord {
    #[key]
    key: String,
    value: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "agents"]
struct AgentRecordRow {
    #[key]
    id: String,
    parent_id: Option<String>,
    task_id: Option<String>,
    project_id: Option<String>,
    role: Option<String>,
    name: String,
    status: String,
    container_id: Option<String>,
    docker_image: String,
    provider_id: String,
    provider_name: String,
    model: String,
    reasoning_effort: Option<String>,
    created_at: String,
    updated_at: String,
    current_turn: Option<String>,
    last_error: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
    system_prompt: Option<String>,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "projects"]
struct ProjectRecordRow {
    #[key]
    id: String,
    name: String,
    status: String,
    owner: String,
    repo: String,
    repository_full_name: String,
    git_account_id: Option<String>,
    repository_id: i64,
    installation_id: i64,
    installation_account: String,
    branch: String,
    docker_image: String,
    clone_status: String,
    maintainer_agent_id: String,
    created_at: String,
    updated_at: String,
    last_error: Option<String>,
    auto_review_enabled: bool,
    reviewer_extra_prompt: Option<String>,
    review_status: String,
    current_reviewer_agent_id: Option<String>,
    last_review_started_at: Option<String>,
    last_review_finished_at: Option<String>,
    next_review_at: Option<String>,
    last_review_outcome: Option<String>,
    review_last_error: Option<String>,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "tasks"]
struct TaskRecordRow {
    #[key]
    id: String,
    title: String,
    status: String,
    planner_agent_id: String,
    current_agent_id: Option<String>,
    created_at: String,
    updated_at: String,
    last_error: Option<String>,
    final_report: Option<String>,
    plan_status: String,
    plan_title: Option<String>,
    plan_markdown: Option<String>,
    plan_version: i64,
    plan_saved_by_agent_id: Option<String>,
    plan_saved_at: Option<String>,
    plan_approved_at: Option<String>,
    plan_revision_feedback: Option<String>,
    plan_revision_requested_at: Option<String>,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "task_reviews"]
struct TaskReviewRecord {
    #[key]
    id: String,
    #[index]
    task_id: String,
    reviewer_agent_id: String,
    round: i64,
    passed: bool,
    findings: String,
    summary: String,
    created_at: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "project_review_runs"]
struct ProjectReviewRunRecord {
    #[key]
    id: String,
    #[index]
    project_id: String,
    reviewer_agent_id: Option<String>,
    turn_id: Option<String>,
    #[index]
    started_at: String,
    finished_at: Option<String>,
    status: String,
    outcome: Option<String>,
    pr: Option<i64>,
    summary: Option<String>,
    error: Option<String>,
    messages_json: String,
    events_json: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "plan_history"]
struct PlanHistoryRecord {
    #[key]
    id: String,
    #[index]
    task_id: String,
    version: i64,
    title: Option<String>,
    markdown: Option<String>,
    saved_at: Option<String>,
    saved_by_agent_id: Option<String>,
    revision_feedback: Option<String>,
    revision_requested_at: Option<String>,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "agent_sessions"]
struct AgentSessionRecord {
    #[key]
    id: String,
    #[index]
    agent_id: String,
    title: String,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "agent_messages"]
struct AgentMessageRecord {
    #[key]
    id: String,
    #[index]
    agent_id: String,
    #[index]
    session_id: String,
    position: i64,
    role: String,
    content: String,
    created_at: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "agent_history_items"]
struct AgentHistoryRecord {
    #[key]
    id: String,
    #[index]
    agent_id: String,
    #[index]
    session_id: String,
    position: i64,
    item_json: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "service_events"]
struct ServiceEventRecord {
    #[key]
    sequence: i64,
    timestamp: String,
    agent_id: Option<String>,
    session_id: Option<String>,
    event_json: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "agent_log_entries"]
struct AgentLogRecord {
    #[key]
    id: String,
    #[index]
    agent_id: String,
    #[index]
    session_id: Option<String>,
    #[index]
    turn_id: Option<String>,
    level: String,
    #[index]
    category: String,
    message: String,
    details_json: String,
    #[index]
    timestamp: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "tool_trace_records"]
struct ToolTraceRecord {
    #[key]
    id: String,
    #[index]
    call_id: String,
    #[index]
    agent_id: String,
    #[index]
    session_id: Option<String>,
    #[index]
    turn_id: Option<String>,
    tool_name: String,
    arguments_json: String,
    output: String,
    success: bool,
    duration_ms: Option<i64>,
    #[index]
    started_at: String,
    completed_at: Option<String>,
    output_preview: String,
}

impl ConfigStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_config_path(path, Self::default_config_path()?).await
    }

    pub async fn open_with_config_path(
        path: impl AsRef<Path>,
        config_path: impl AsRef<Path>,
    ) -> Result<Self> {
        let artifact_index_dir = Self::default_artifact_index_dir()?;
        Self::open_with_config_and_artifact_index_path(path, config_path, artifact_index_dir).await
    }

    pub async fn open_with_config_and_artifact_index_path(
        path: impl AsRef<Path>,
        config_path: impl AsRef<Path>,
        artifact_index_dir: impl AsRef<Path>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let config_path = config_path.as_ref().to_path_buf();
        let artifact_index_dir = artifact_index_dir.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut was_empty =
            !path.exists() || path.metadata().is_ok_and(|metadata| metadata.len() == 0);
        if !was_empty && !has_sqlite_header(&path)? {
            let _ = std::fs::remove_file(&path);
            was_empty = true;
        }

        let mut db = build_db(&path).await?;
        if was_empty {
            db.push_schema().await?;
            set_setting_on(&mut db, SETTING_SCHEMA_VERSION, SCHEMA_VERSION).await?;
        } else {
            let current_schema_version = get_setting_on(&db, SETTING_SCHEMA_VERSION)
                .await
                .ok()
                .flatten();
            if current_schema_version.as_deref() != Some(SCHEMA_VERSION) {
                if matches!(
                    current_schema_version.as_deref(),
                    Some("8" | "9" | "10" | "11" | "12" | "13" | "14" | "15")
                ) {
                    drop(db);
                    migrate_to_v16(&path)?;
                    db = build_db(&path).await?;
                    set_setting_on(&mut db, SETTING_SCHEMA_VERSION, SCHEMA_VERSION).await?;
                } else {
                    drop(db);
                    let _ = std::fs::remove_file(&path);
                    db = build_db(&path).await?;
                    db.push_schema().await?;
                    set_setting_on(&mut db, SETTING_SCHEMA_VERSION, SCHEMA_VERSION).await?;
                }
            }
        }

        let store = Self {
            path,
            config_path,
            artifact_index_dir,
            db,
            git_accounts_lock: Mutex::new(()),
            providers_cache: Mutex::new(None),
        };
        Ok(store)
    }

    pub fn default_path() -> Result<PathBuf> {
        let home = dirs::home_dir()
            .ok_or_else(|| StoreError::InvalidConfig("home directory not found".to_string()))?;
        Ok(home.join(".mai-team").join("mai-team.sqlite3"))
    }

    pub fn default_config_path() -> Result<PathBuf> {
        if let Ok(path) = std::env::var("MAI_CONFIG_PATH") {
            return Ok(PathBuf::from(path));
        }
        let home = dirs::home_dir()
            .ok_or_else(|| StoreError::InvalidConfig("home directory not found".to_string()))?;
        Ok(home.join(".mai-team").join("config.toml"))
    }

    pub fn default_artifact_index_dir() -> Result<PathBuf> {
        let data_dir = std::env::var("MAI_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|_| {
                dirs::home_dir()
                    .map(|home| home.join(".mai-team"))
                    .ok_or(std::env::VarError::NotPresent)
            });
        data_dir
            .map(|path| path.join("artifacts").join("index"))
            .map_err(|_| StoreError::InvalidConfig("home directory not found".to_string()))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn artifact_index_dir(&self) -> &Path {
        &self.artifact_index_dir
    }

    pub async fn seed_default_provider_from_env(
        &self,
        api_key: Option<String>,
        base_url: String,
        model: String,
    ) -> Result<()> {
        if self.provider_count().await? > 0 {
            return Ok(());
        }
        let Some(api_key) = api_key.filter(|value| !value.trim().is_empty()) else {
            return Ok(());
        };
        let mut provider = builtin_provider(ProviderKind::Openai);
        provider.base_url = base_url;
        provider.api_key = Some(api_key);
        if provider.models.iter().all(|item| item.id != model) {
            provider.models.insert(0, fallback_model(&model));
        }
        provider.default_model = model;
        self.save_providers(ProvidersConfigRequest {
            default_provider_id: Some("openai".to_string()),
            providers: vec![provider],
        })
        .await
    }

    pub async fn provider_count(&self) -> Result<usize> {
        Ok(self.load_providers_toml().await?.providers.len())
    }

    pub fn provider_presets_response(&self) -> ProviderPresetsResponse {
        ProviderPresetsResponse {
            providers: vec![
                provider_preset(ProviderKind::Openai),
                provider_preset(ProviderKind::Deepseek),
                provider_preset_from_config(mimo_builtin_provider(
                    "mimo-api",
                    "MiMo API",
                    "https://api.xiaomimimo.com/v1",
                    "MIMO_API_KEY",
                )),
                provider_preset_from_config(mimo_builtin_provider(
                    "mimo-token-plan",
                    "MiMo Token Plan",
                    "https://token-plan-cn.xiaomimimo.com/v1",
                    "MIMO_TOKEN_PLAN_API_KEY",
                )),
            ],
        }
    }

    pub async fn providers_response(&self) -> Result<ProvidersResponse> {
        let providers = self.list_provider_secrets().await?;
        let file = self.load_providers_toml().await?;
        Ok(ProvidersResponse {
            providers: providers
                .into_iter()
                .map(|provider| ProviderSummary {
                    id: provider.id,
                    kind: provider.kind,
                    name: provider.name,
                    base_url: provider.base_url,
                    api_key_env: provider.api_key_env,
                    models: provider.models,
                    default_model: provider.default_model,
                    enabled: provider.enabled,
                    has_api_key: !provider.api_key.is_empty(),
                })
                .collect(),
            default_provider_id: file.default_provider_id,
        })
    }

    pub async fn save_providers(&self, request: ProvidersConfigRequest) -> Result<()> {
        validate_provider_request(&request)?;
        let existing = self
            .load_providers_toml()
            .await
            .unwrap_or_else(|_| ProvidersToml::default());
        let mut file = ProvidersToml::default();
        for provider in &request.providers {
            let provider_id = normalized_id(&provider.id);
            let existing_provider = existing.providers.get(&provider_id);
            let api_key = provider
                .api_key
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
                .or_else(|| existing_provider.and_then(|item| item.api_key.clone()));
            let mut models = BTreeMap::new();
            for model in normalized_models(provider) {
                models.insert(model.id.clone(), ModelToml::from_model(model));
            }
            file.providers.insert(
                provider_id,
                ProviderToml {
                    kind: provider.kind,
                    name: provider.name.trim().to_string(),
                    base_url: provider.base_url.trim().to_string(),
                    api_key,
                    api_key_env: provider
                        .api_key_env
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                        .map(str::to_string),
                    default_model: provider.default_model.trim().to_string(),
                    enabled: provider.enabled,
                    models,
                },
            );
        }

        if let Some(default_provider_id) = request.default_provider_id.as_deref() {
            file.default_provider_id = Some(default_provider_id.trim().to_string());
        } else if let Some(first) = request.providers.first() {
            file.default_provider_id = Some(first.id.trim().to_string());
        }

        self.write_providers_toml(&file)?;
        self.clear_providers_cache().await;
        Ok(())
    }

    pub async fn resolve_provider(
        &self,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<ProviderSelection> {
        let provider = match provider_id.filter(|value| !value.trim().is_empty()) {
            Some(id) => self
                .get_provider_secret(id)
                .await?
                .ok_or_else(|| StoreError::InvalidConfig(format!("provider `{id}` not found")))?,
            None => self.default_provider_secret().await?.ok_or_else(|| {
                StoreError::InvalidConfig(
                    "no provider configured; add one in the Providers page".to_string(),
                )
            })?,
        };
        if !provider.enabled {
            return Err(StoreError::InvalidConfig(format!(
                "provider `{}` is disabled",
                provider.id
            )));
        }
        if provider.api_key.trim().is_empty() {
            return Err(StoreError::InvalidConfig(format!(
                "provider `{}` has no API key",
                provider.id
            )));
        }
        let selected_model_id = model
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| provider.default_model.clone());
        let selected_model = provider
            .models
            .iter()
            .find(|item| item.id == selected_model_id)
            .cloned()
            .ok_or_else(|| {
                StoreError::InvalidConfig(format!(
                    "model `{selected_model_id}` is not configured for provider `{}`",
                    provider.id
                ))
            })?;
        if selected_model.context_tokens == 0 || selected_model.output_tokens == 0 {
            return Err(StoreError::InvalidConfig(format!(
                "model `{selected_model_id}` must configure context_tokens and output_tokens"
            )));
        }
        Ok(ProviderSelection {
            provider,
            model: selected_model,
        })
    }

    pub async fn get_provider_secret(&self, id: &str) -> Result<Option<ProviderSecret>> {
        let providers = self.list_provider_secrets().await?;
        Ok(providers.into_iter().find(|provider| provider.id == id))
    }

    pub async fn default_provider_secret(&self) -> Result<Option<ProviderSecret>> {
        let providers = self.list_provider_secrets().await?;
        if providers.is_empty() {
            return Ok(None);
        }
        if let Some(default_provider_id) = self.load_providers_toml().await?.default_provider_id
            && let Some(provider) = providers
                .iter()
                .find(|provider| provider.id == default_provider_id && provider.enabled)
        {
            return Ok(Some(provider.clone()));
        }
        Ok(providers.into_iter().find(|provider| provider.enabled))
    }

    pub async fn list_provider_secrets(&self) -> Result<Vec<ProviderSecret>> {
        let file = self.load_providers_toml().await?;
        let mut out = Vec::with_capacity(file.providers.len());
        for (id, provider) in file.providers {
            out.push(ProviderSecret {
                models: provider
                    .models
                    .into_iter()
                    .map(|(id, model)| model.into_model(id, provider.kind))
                    .collect(),
                id,
                kind: provider.kind,
                name: provider.name,
                base_url: provider.base_url,
                api_key: resolve_api_key(provider.api_key, provider.api_key_env.as_deref()),
                api_key_env: provider.api_key_env,
                default_model: provider.default_model,
                enabled: provider.enabled,
            });
        }
        Ok(out)
    }

    async fn load_providers_toml(&self) -> Result<ProvidersToml> {
        let stamp = providers_cache_stamp(&self.config_path)?;
        if let Some(cache) = self.providers_cache.lock().await.as_ref()
            && cache.stamp == stamp
        {
            return Ok(cache.file.clone());
        }

        let (file, stamp) = self.read_providers_toml_with_stamp(stamp)?;
        *self.providers_cache.lock().await = Some(ProvidersCache {
            stamp,
            file: file.clone(),
        });
        Ok(file)
    }

    fn read_providers_toml_with_stamp(
        &self,
        stamp: ProvidersCacheStamp,
    ) -> Result<(ProvidersToml, ProvidersCacheStamp)> {
        if !stamp.exists {
            return Ok((ProvidersToml::default(), stamp));
        }
        let text = match std::fs::read_to_string(&self.config_path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok((
                    ProvidersToml::default(),
                    providers_cache_stamp(&self.config_path)?,
                ));
            }
            Err(err) => return Err(err.into()),
        };
        match toml::from_str::<ProvidersToml>(&text) {
            Ok(file) => match validate_providers_toml(&file) {
                Ok(()) => Ok((file, providers_cache_stamp(&self.config_path)?)),
                Err(_) => {
                    let _ = std::fs::remove_file(&self.config_path);
                    Ok((
                        ProvidersToml::default(),
                        providers_cache_stamp(&self.config_path)?,
                    ))
                }
            },
            Err(_) => {
                let _ = std::fs::remove_file(&self.config_path);
                Ok((
                    ProvidersToml::default(),
                    providers_cache_stamp(&self.config_path)?,
                ))
            }
        }
    }

    fn write_providers_toml(&self, file: &ProvidersToml) -> Result<()> {
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(file)?;
        std::fs::write(&self.config_path, text)?;
        Ok(())
    }

    async fn clear_providers_cache(&self) {
        *self.providers_cache.lock().await = None;
    }

    pub async fn list_mcp_servers(&self) -> Result<BTreeMap<String, McpServerConfig>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<McpServerRecord>>::all().exec(&mut db).await?;
        rows.sort_by(|left, right| {
            left.sort_order
                .cmp(&right.sort_order)
                .then_with(|| left.name.cmp(&right.name))
        });

        let mut servers = BTreeMap::new();
        for row in rows {
            let mut config = serde_json::from_str::<McpServerConfig>(&row.config_json)?;
            config.enabled = row.enabled;
            servers.insert(row.name.clone(), config);
        }
        Ok(servers)
    }

    pub async fn save_mcp_servers(
        &self,
        servers: &BTreeMap<String, McpServerConfig>,
    ) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<McpServerRecord>>::all()
            .delete()
            .exec(&mut tx)
            .await?;

        for (index, (name, config)) in servers.iter().enumerate() {
            toasty::create!(McpServerRecord {
                name: name.clone(),
                config_json: serde_json::to_string(config)?,
                enabled: config.enabled,
                sort_order: index as i64,
            })
            .exec(&mut tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        get_setting_on(&self.db, key).await
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let mut db = self.db.clone();
        set_setting_on(&mut db, key, value).await
    }

    pub async fn load_agent_config(&self) -> Result<AgentConfigRequest> {
        let Some(value) = self.get_setting(SETTING_AGENT_CONFIG).await? else {
            return Ok(AgentConfigRequest::default());
        };
        match serde_json::from_str(&value) {
            Ok(config) => Ok(config),
            Err(_) => {
                let mut db = self.db.clone();
                let mut tx = db.transaction().await?;
                delete_setting_in_tx(&mut tx, SETTING_AGENT_CONFIG).await?;
                tx.commit().await?;
                Ok(AgentConfigRequest::default())
            }
        }
    }

    pub async fn save_agent_config(&self, config: &AgentConfigRequest) -> Result<()> {
        self.set_setting(SETTING_AGENT_CONFIG, &serde_json::to_string(config)?)
            .await
    }

    pub async fn load_skills_config(&self) -> Result<SkillsConfigRequest> {
        let Some(value) = self.get_setting(SETTING_SKILLS_CONFIG).await? else {
            return Ok(SkillsConfigRequest::default());
        };
        match serde_json::from_str(&value) {
            Ok(config) => Ok(config),
            Err(_) => {
                let mut db = self.db.clone();
                let mut tx = db.transaction().await?;
                delete_setting_in_tx(&mut tx, SETTING_SKILLS_CONFIG).await?;
                tx.commit().await?;
                Ok(SkillsConfigRequest::default())
            }
        }
    }

    pub async fn save_skills_config(&self, config: &SkillsConfigRequest) -> Result<()> {
        self.set_setting(SETTING_SKILLS_CONFIG, &serde_json::to_string(config)?)
            .await
    }

    pub async fn get_github_settings(&self) -> Result<GithubSettingsResponse> {
        let has_token = self.get_setting(SETTING_GITHUB_TOKEN).await?.is_some();
        Ok(GithubSettingsResponse { has_token })
    }

    pub async fn save_github_token(&self, token: &str) -> Result<GithubSettingsResponse> {
        self.set_setting(SETTING_GITHUB_TOKEN, token).await?;
        Ok(GithubSettingsResponse { has_token: true })
    }

    pub async fn clear_github_token(&self) -> Result<GithubSettingsResponse> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        delete_setting_in_tx(&mut tx, SETTING_GITHUB_TOKEN).await?;
        tx.commit().await?;
        Ok(GithubSettingsResponse { has_token: false })
    }

    pub async fn list_git_accounts(&self) -> Result<GitAccountsResponse> {
        let _guard = self.git_accounts_lock.lock().await;
        let config = self.git_accounts_config().await?;
        Ok(git_accounts_response(&config))
    }

    pub async fn upsert_git_account(
        &self,
        request: GitAccountRequest,
    ) -> Result<GitAccountSummary> {
        let _guard = self.git_accounts_lock.lock().await;
        let token = request.token.unwrap_or_default().trim().to_string();
        let mut config = self.git_accounts_config().await?;
        let id = request
            .id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let existing = config.accounts.iter().position(|account| account.id == id);
        let current_token = existing
            .and_then(|index| config.accounts.get(index))
            .map(|account| account.token_secret.clone())
            .unwrap_or_default();
        let has_new_token = !token.is_empty();
        let token_secret = if token.is_empty() {
            current_token
        } else {
            token
        };
        if token_secret.trim().is_empty() {
            return Err(StoreError::InvalidConfig(
                "git account token is required".to_string(),
            ));
        }
        let fallback_label = request
            .login
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("GitHub");
        let label = request
            .label
            .trim()
            .to_string()
            .if_empty(fallback_label)
            .to_string();
        let mut account = existing
            .and_then(|index| config.accounts.get(index).cloned())
            .unwrap_or_else(|| StoredGitAccount {
                id: id.clone(),
                provider: request.provider.clone(),
                label: label.clone(),
                login: None,
                token_kind: GitTokenKind::Unknown,
                scopes: Vec::new(),
                status: GitAccountStatus::Unverified,
                is_default: false,
                token_secret: token_secret.clone(),
                last_verified_at: None,
                last_error: None,
            });
        account.provider = request.provider;
        account.label = label;
        if let Some(login) = request.login {
            account.login = Some(login.trim().to_string()).filter(|value| !value.is_empty());
        }
        account.token_secret = token_secret;
        account.status = GitAccountStatus::Verifying;
        account.last_error = None;
        if has_new_token {
            account.last_verified_at = None;
        }
        if request.is_default || config.accounts.is_empty() {
            config.default_account_id = Some(id.clone());
        }
        account.is_default = config.default_account_id.as_deref() == Some(id.as_str());
        if let Some(index) = existing {
            config.accounts[index] = account;
        } else {
            config.accounts.push(account);
        }
        normalize_git_account_defaults(&mut config);
        self.save_git_accounts_config(&config).await?;
        Ok(config
            .accounts
            .iter()
            .find(|account| account.id == id)
            .map(|account| account.summary(config.default_account_id.as_deref()))
            .expect("saved account"))
    }

    pub async fn update_git_account_verification(
        &self,
        account_id: &str,
        login: Option<String>,
        token_kind: GitTokenKind,
        scopes: Vec<String>,
        status: GitAccountStatus,
        last_error: Option<String>,
    ) -> Result<GitAccountSummary> {
        let _guard = self.git_accounts_lock.lock().await;
        let mut config = self.git_accounts_config().await?;
        let default_account_id = config.default_account_id.clone();
        let account = config
            .accounts
            .iter_mut()
            .find(|account| account.id == account_id)
            .ok_or_else(|| StoreError::InvalidConfig("git account not found".to_string()))?;
        account.login = login.or_else(|| account.login.clone());
        account.token_kind = token_kind;
        account.scopes = scopes;
        account.status = status;
        account.last_verified_at = Some(Utc::now());
        account.last_error = last_error;
        let summary = account.summary(default_account_id.as_deref());
        self.save_git_accounts_config(&config).await?;
        Ok(summary)
    }

    pub async fn mark_git_account_verifying(&self, account_id: &str) -> Result<GitAccountSummary> {
        let _guard = self.git_accounts_lock.lock().await;
        let mut config = self.git_accounts_config().await?;
        let default_account_id = config.default_account_id.clone();
        let account = config
            .accounts
            .iter_mut()
            .find(|account| account.id == account_id)
            .ok_or_else(|| StoreError::InvalidConfig("git account not found".to_string()))?;
        account.status = GitAccountStatus::Verifying;
        account.last_error = None;
        let summary = account.summary(default_account_id.as_deref());
        self.save_git_accounts_config(&config).await?;
        Ok(summary)
    }

    pub async fn delete_git_account(&self, account_id: &str) -> Result<GitAccountsResponse> {
        let _guard = self.git_accounts_lock.lock().await;
        let mut config = self.git_accounts_config().await?;
        config.accounts.retain(|account| account.id != account_id);
        if config.default_account_id.as_deref() == Some(account_id) {
            config.default_account_id = config.accounts.first().map(|account| account.id.clone());
        }
        normalize_git_account_defaults(&mut config);
        self.save_git_accounts_config(&config).await?;
        Ok(git_accounts_response(&config))
    }

    pub async fn set_default_git_account(&self, account_id: &str) -> Result<GitAccountsResponse> {
        let _guard = self.git_accounts_lock.lock().await;
        let mut config = self.git_accounts_config().await?;
        if !config
            .accounts
            .iter()
            .any(|account| account.id == account_id)
        {
            return Err(StoreError::InvalidConfig(
                "git account not found".to_string(),
            ));
        }
        config.default_account_id = Some(account_id.to_string());
        normalize_git_account_defaults(&mut config);
        self.save_git_accounts_config(&config).await?;
        Ok(git_accounts_response(&config))
    }

    pub async fn git_account_token(&self, account_id: &str) -> Result<Option<String>> {
        let _guard = self.git_accounts_lock.lock().await;
        Ok(self
            .git_accounts_config()
            .await?
            .accounts
            .into_iter()
            .find(|account| account.id == account_id)
            .map(|account| account.token_secret))
    }

    async fn git_accounts_config(&self) -> Result<GitAccountsConfig> {
        let mut config = match self.get_setting(SETTING_GIT_ACCOUNTS).await? {
            Some(value) if !value.trim().is_empty() => serde_json::from_str(&value)?,
            _ => GitAccountsConfig::default(),
        };
        if config.accounts.is_empty()
            && let Some(token) = self.get_setting(SETTING_GITHUB_TOKEN).await?
        {
            let token = token.trim().to_string();
            if !token.is_empty() {
                config.accounts.push(StoredGitAccount {
                    id: "github-default".to_string(),
                    provider: GitProvider::Github,
                    label: "GitHub".to_string(),
                    login: None,
                    token_kind: GitTokenKind::Unknown,
                    scopes: Vec::new(),
                    status: GitAccountStatus::Unverified,
                    is_default: true,
                    token_secret: token,
                    last_verified_at: None,
                    last_error: None,
                });
                config.default_account_id = Some("github-default".to_string());
            }
        }
        normalize_git_account_defaults(&mut config);
        Ok(config)
    }

    async fn save_git_accounts_config(&self, config: &GitAccountsConfig) -> Result<()> {
        self.set_setting(SETTING_GIT_ACCOUNTS, &serde_json::to_string(config)?)
            .await
    }

    pub async fn get_github_app_settings(&self) -> Result<GithubAppSettingsResponse> {
        let config = self.github_app_config().await?;
        Ok(GithubAppSettingsResponse {
            app_id: config.app_id.clone(),
            base_url: config
                .base_url
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_GITHUB_API_BASE_URL.to_string()),
            has_private_key: config
                .private_key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty()),
            app_slug: config.app_slug.clone(),
            app_html_url: config.app_html_url.clone(),
            owner_login: config.owner_login.clone(),
            owner_type: config.owner_type.clone(),
            install_url: github_app_install_url(config.app_slug.as_deref()),
        })
    }

    pub async fn github_app_secret(&self) -> Result<Option<(String, String, String)>> {
        let config = self.github_app_config().await?;
        let app_id = config.app_id.filter(|value| !value.trim().is_empty());
        let private_key = config.private_key.filter(|value| !value.trim().is_empty());
        match (app_id, private_key) {
            (Some(app_id), Some(private_key)) => Ok(Some((
                app_id,
                private_key,
                config
                    .base_url
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_GITHUB_API_BASE_URL.to_string()),
            ))),
            _ => Ok(None),
        }
    }

    pub async fn save_github_app_settings(
        &self,
        request: GithubAppSettingsRequest,
    ) -> Result<GithubAppSettingsResponse> {
        let mut current = self.github_app_config().await?;
        if let Some(app_id) = request.app_id {
            current.app_id = Some(app_id.trim().to_string()).filter(|value| !value.is_empty());
        }
        if let Some(private_key) = request.private_key {
            current.private_key =
                Some(private_key.trim().to_string()).filter(|value| !value.is_empty());
        }
        if let Some(base_url) = request.base_url {
            current.base_url = Some(base_url.trim().trim_end_matches('/').to_string())
                .filter(|value| !value.is_empty());
        }
        if let Some(app_slug) = request.app_slug {
            current.app_slug = Some(app_slug.trim().to_string()).filter(|value| !value.is_empty());
        }
        if let Some(app_html_url) = request.app_html_url {
            current.app_html_url =
                Some(app_html_url.trim().to_string()).filter(|value| !value.is_empty());
        }
        if let Some(owner_login) = request.owner_login {
            current.owner_login =
                Some(owner_login.trim().to_string()).filter(|value| !value.is_empty());
        }
        if let Some(owner_type) = request.owner_type {
            current.owner_type =
                Some(owner_type.trim().to_string()).filter(|value| !value.is_empty());
        }
        self.set_setting(SETTING_GITHUB_APP_CONFIG, &serde_json::to_string(&current)?)
            .await?;
        self.get_github_app_settings().await
    }

    async fn github_app_config(&self) -> Result<GithubAppConfig> {
        match self.get_setting(SETTING_GITHUB_APP_CONFIG).await? {
            Some(value) if !value.trim().is_empty() => Ok(serde_json::from_str(&value)?),
            _ => Ok(GithubAppConfig::default()),
        }
    }

    pub async fn save_agent(
        &self,
        summary: &AgentSummary,
        system_prompt: Option<&str>,
    ) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        delete_agent_row_in_tx(&mut tx, summary.id).await?;
        toasty::create!(AgentRecordRow {
            id: summary.id.to_string(),
            parent_id: summary.parent_id.map(|id| id.to_string()),
            task_id: summary.task_id.map(|id| id.to_string()),
            project_id: summary.project_id.map(|id| id.to_string()),
            role: summary.role.map(|r| r.to_string()),
            name: summary.name.clone(),
            status: summary.status.to_string(),
            container_id: summary.container_id.clone(),
            docker_image: summary.docker_image.clone(),
            provider_id: summary.provider_id.clone(),
            provider_name: summary.provider_name.clone(),
            model: summary.model.clone(),
            reasoning_effort: summary.reasoning_effort.clone(),
            created_at: summary.created_at.to_rfc3339(),
            updated_at: summary.updated_at.to_rfc3339(),
            current_turn: summary.current_turn.map(|id| id.to_string()),
            last_error: summary.last_error.clone(),
            input_tokens: u64_to_i64(summary.token_usage.input_tokens),
            output_tokens: u64_to_i64(summary.token_usage.output_tokens),
            total_tokens: u64_to_i64(summary.token_usage.total_tokens),
            system_prompt: system_prompt.map(str::to_string),
        })
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn save_project(&self, project: &ProjectSummary) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<ProjectRecordRow>>::filter(
            ProjectRecordRow::fields().id().eq(project.id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        toasty::create!(ProjectRecordRow {
            id: project.id.to_string(),
            name: project.name.clone(),
            status: project.status.to_string(),
            owner: project.owner.clone(),
            repo: project.repo.clone(),
            repository_full_name: project.repository_full_name.clone(),
            git_account_id: project.git_account_id.clone(),
            repository_id: u64_to_i64(project.repository_id),
            installation_id: u64_to_i64(project.installation_id),
            installation_account: project.installation_account.clone(),
            branch: project.branch.clone(),
            docker_image: project.docker_image.clone(),
            clone_status: project.clone_status.to_string(),
            maintainer_agent_id: project.maintainer_agent_id.to_string(),
            created_at: project.created_at.to_rfc3339(),
            updated_at: project.updated_at.to_rfc3339(),
            last_error: project.last_error.clone(),
            auto_review_enabled: project.auto_review_enabled,
            reviewer_extra_prompt: project.reviewer_extra_prompt.clone(),
            review_status: project.review_status.to_string(),
            current_reviewer_agent_id: project.current_reviewer_agent_id.map(|id| id.to_string()),
            last_review_started_at: project.last_review_started_at.map(|time| time.to_rfc3339()),
            last_review_finished_at: project
                .last_review_finished_at
                .map(|time| time.to_rfc3339()),
            next_review_at: project.next_review_at.map(|time| time.to_rfc3339()),
            last_review_outcome: project.last_review_outcome.as_ref().map(|o| o.to_string()),
            review_last_error: project.review_last_error.clone(),
        })
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_project(&self, project_id: ProjectId) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<ProjectRecordRow>>::filter(
            ProjectRecordRow::fields().id().eq(project_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        Query::<List<ProjectReviewRunRecord>>::filter(
            ProjectReviewRunRecord::fields()
                .project_id()
                .eq(project_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn load_projects(&self) -> Result<Vec<ProjectSummary>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<ProjectRecordRow>>::all().exec(&mut db).await?;
        rows.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        rows.into_iter()
            .map(ProjectRecordRow::into_summary)
            .collect()
    }

    pub async fn save_task(&self, task: &TaskSummary, plan: &TaskPlan) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<TaskRecordRow>>::filter(TaskRecordRow::fields().id().eq(task.id.to_string()))
            .delete()
            .exec(&mut tx)
            .await?;
        toasty::create!(TaskRecordRow {
            id: task.id.to_string(),
            title: task.title.clone(),
            status: task.status.to_string(),
            planner_agent_id: task.planner_agent_id.to_string(),
            current_agent_id: task.current_agent_id.map(|id| id.to_string()),
            created_at: task.created_at.to_rfc3339(),
            updated_at: task.updated_at.to_rfc3339(),
            last_error: task.last_error.clone(),
            final_report: task.final_report.clone(),
            plan_status: plan.status.to_string(),
            plan_title: plan.title.clone(),
            plan_markdown: plan.markdown.clone(),
            plan_version: u64_to_i64(plan.version),
            plan_saved_by_agent_id: plan.saved_by_agent_id.map(|id| id.to_string()),
            plan_saved_at: plan.saved_at.map(|time| time.to_rfc3339()),
            plan_approved_at: plan.approved_at.map(|time| time.to_rfc3339()),
            plan_revision_feedback: plan.revision_feedback.clone(),
            plan_revision_requested_at: plan.revision_requested_at.map(|time| time.to_rfc3339()),
        })
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_task(&self, task_id: TaskId) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<TaskRecordRow>>::filter(TaskRecordRow::fields().id().eq(task_id.to_string()))
            .delete()
            .exec(&mut tx)
            .await?;
        Query::<List<TaskReviewRecord>>::filter(
            TaskReviewRecord::fields().task_id().eq(task_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        Query::<List<PlanHistoryRecord>>::filter(
            PlanHistoryRecord::fields()
                .task_id()
                .eq(task_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn append_task_review(&self, review: &TaskReview) -> Result<()> {
        let mut db = self.db.clone();
        Query::<List<TaskReviewRecord>>::filter(
            TaskReviewRecord::fields().id().eq(review.id.to_string()),
        )
        .delete()
        .exec(&mut db)
        .await?;
        toasty::create!(TaskReviewRecord {
            id: review.id.to_string(),
            task_id: review.task_id.to_string(),
            reviewer_agent_id: review.reviewer_agent_id.to_string(),
            round: u64_to_i64(review.round),
            passed: review.passed,
            findings: review.findings.clone(),
            summary: review.summary.clone(),
            created_at: review.created_at.to_rfc3339(),
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn save_project_review_run(&self, run: &ProjectReviewRunDetail) -> Result<()> {
        let mut db = self.db.clone();
        Query::<List<ProjectReviewRunRecord>>::filter(
            ProjectReviewRunRecord::fields()
                .id()
                .eq(run.summary.id.to_string()),
        )
        .delete()
        .exec(&mut db)
        .await?;
        toasty::create!(ProjectReviewRunRecord {
            id: run.summary.id.to_string(),
            project_id: run.summary.project_id.to_string(),
            reviewer_agent_id: run.summary.reviewer_agent_id.map(|id| id.to_string()),
            turn_id: run.summary.turn_id.map(|id| id.to_string()),
            started_at: run.summary.started_at.to_rfc3339(),
            finished_at: run.summary.finished_at.map(|time| time.to_rfc3339()),
            status: run.summary.status.to_string(),
            outcome: run
                .summary
                .outcome
                .as_ref()
                .map(|outcome| outcome.to_string()),
            pr: run.summary.pr.map(u64_to_i64),
            summary: run.summary.summary.clone(),
            error: run.summary.error.clone(),
            messages_json: serde_json::to_string(&run.messages)?,
            events_json: serde_json::to_string(&run.events)?,
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn load_project_review_runs(
        &self,
        project_id: ProjectId,
        since: Option<DateTime<Utc>>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<ProjectReviewRunSummary>> {
        let mut rows = self.load_project_review_run_records(project_id).await?;
        if let Some(since) = since {
            let cutoff = since.to_rfc3339();
            rows.retain(|row| row.started_at >= cutoff);
        }
        rows.sort_by(|left, right| {
            right
                .started_at
                .cmp(&left.started_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        let limit = limit.max(1);
        rows.into_iter()
            .skip(offset)
            .take(limit)
            .map(ProjectReviewRunRecord::into_summary)
            .collect()
    }

    pub async fn load_project_review_run(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
    ) -> Result<Option<ProjectReviewRunDetail>> {
        let mut db = self.db.clone();
        let row = Query::<List<ProjectReviewRunRecord>>::filter(
            ProjectReviewRunRecord::fields().id().eq(run_id.to_string()),
        )
        .first()
        .exec(&mut db)
        .await?;
        row.filter(|row| row.project_id == project_id.to_string())
            .map(ProjectReviewRunRecord::into_detail)
            .transpose()
    }

    pub async fn prune_project_review_runs_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        let mut db = self.db.clone();
        let cutoff = cutoff.to_rfc3339();
        let rows = Query::<List<ProjectReviewRunRecord>>::all()
            .exec(&mut db)
            .await?;
        let old_ids = rows
            .into_iter()
            .filter(|row| row.started_at < cutoff)
            .map(|row| row.id)
            .collect::<Vec<_>>();
        for id in &old_ids {
            Query::<List<ProjectReviewRunRecord>>::filter(
                ProjectReviewRunRecord::fields().id().eq(id.clone()),
            )
            .delete()
            .exec(&mut db)
            .await?;
        }
        Ok(old_ids.len())
    }

    async fn load_project_review_run_records(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<ProjectReviewRunRecord>> {
        let mut db = self.db.clone();
        Ok(Query::<List<ProjectReviewRunRecord>>::filter(
            ProjectReviewRunRecord::fields()
                .project_id()
                .eq(project_id.to_string()),
        )
        .exec(&mut db)
        .await?)
    }

    pub async fn save_plan_history_entry(
        &self,
        task_id: TaskId,
        entry: &PlanHistoryEntry,
    ) -> Result<()> {
        let mut db = self.db.clone();
        toasty::create!(PlanHistoryRecord {
            id: Uuid::new_v4().to_string(),
            task_id: task_id.to_string(),
            version: u64_to_i64(entry.version),
            title: entry.title.clone(),
            markdown: entry.markdown.clone(),
            saved_at: entry.saved_at.map(|time| time.to_rfc3339()),
            saved_by_agent_id: entry.saved_by_agent_id.map(|id| id.to_string()),
            revision_feedback: entry.revision_feedback.clone(),
            revision_requested_at: entry.revision_requested_at.map(|time| time.to_rfc3339()),
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn load_plan_history(&self, task_id: TaskId) -> Result<Vec<PlanHistoryEntry>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<PlanHistoryRecord>>::filter(
            PlanHistoryRecord::fields()
                .task_id()
                .eq(task_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.sort_by_key(|row| row.version);
        rows.into_iter()
            .map(|row| {
                Ok(PlanHistoryEntry {
                    version: i64_to_u64(row.version),
                    title: row.title,
                    markdown: row.markdown,
                    saved_at: row.saved_at.as_deref().map(parse_utc).transpose()?,
                    saved_by_agent_id: row
                        .saved_by_agent_id
                        .as_deref()
                        .map(parse_agent_id)
                        .transpose()?,
                    revision_feedback: row.revision_feedback,
                    revision_requested_at: row
                        .revision_requested_at
                        .as_deref()
                        .map(parse_utc)
                        .transpose()?,
                })
            })
            .collect()
    }

    pub fn save_artifact(&self, info: &ArtifactInfo) -> Result<()> {
        let dir = self.artifact_index_dir();
        std::fs::create_dir_all(dir)?;
        let file = dir.join(format!("{}.json", info.id));
        let data = serde_json::to_string(info)?;
        std::fs::write(file, data)?;
        Ok(())
    }

    pub fn load_artifacts(&self, task_id: &TaskId) -> Result<Vec<ArtifactInfo>> {
        let dir = self.artifact_index_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let data = std::fs::read_to_string(&path)?;
            let info: ArtifactInfo = serde_json::from_str(&data)?;
            if info.task_id == *task_id {
                result.push(info);
            }
        }
        result.sort_by_key(|a| a.created_at);
        Ok(result)
    }

    pub fn load_all_artifacts(&self) -> Result<Vec<ArtifactInfo>> {
        let dir = self.artifact_index_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let data = std::fs::read_to_string(&path)?;
            let info: ArtifactInfo = serde_json::from_str(&data)?;
            result.push(info);
        }
        result.sort_by_key(|a| a.created_at);
        Ok(result)
    }

    pub async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        delete_agent_row_in_tx(&mut tx, agent_id).await?;
        Query::<List<AgentSessionRecord>>::filter(
            AgentSessionRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        Query::<List<AgentMessageRecord>>::filter(
            AgentMessageRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        Query::<List<AgentHistoryRecord>>::filter(
            AgentHistoryRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        Query::<List<AgentLogRecord>>::filter(
            AgentLogRecord::fields().agent_id().eq(agent_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        Query::<List<ToolTraceRecord>>::filter(
            ToolTraceRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn save_agent_session(
        &self,
        agent_id: AgentId,
        session: &AgentSessionSummary,
    ) -> Result<()> {
        let mut db = self.db.clone();
        Query::<List<AgentSessionRecord>>::filter(
            AgentSessionRecord::fields().id().eq(session.id.to_string()),
        )
        .delete()
        .exec(&mut db)
        .await?;
        toasty::create!(AgentSessionRecord {
            id: session.id.to_string(),
            agent_id: agent_id.to_string(),
            title: session.title.clone(),
            created_at: session.created_at.to_rfc3339(),
            updated_at: session.updated_at.to_rfc3339(),
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn append_agent_message(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        position: usize,
        message: &AgentMessage,
    ) -> Result<()> {
        let mut db = self.db.clone();
        toasty::create!(AgentMessageRecord {
            id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            position: position as i64,
            role: message.role.to_string(),
            content: message.content.clone(),
            created_at: message.created_at.to_rfc3339(),
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn append_agent_history_item(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        position: usize,
        item: &ModelInputItem,
    ) -> Result<()> {
        let mut db = self.db.clone();
        toasty::create!(AgentHistoryRecord {
            id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            position: position as i64,
            item_json: serde_json::to_string(item)?,
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn replace_agent_history(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        items: &[ModelInputItem],
    ) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<AgentHistoryRecord>>::filter(
            AgentHistoryRecord::fields()
                .agent_id()
                .eq(agent_id.to_string())
                .and(
                    AgentHistoryRecord::fields()
                        .session_id()
                        .eq(session_id.to_string()),
                ),
        )
        .delete()
        .exec(&mut tx)
        .await?;

        for (position, item) in items.iter().enumerate() {
            toasty::create!(AgentHistoryRecord {
                id: Uuid::new_v4().to_string(),
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                position: position as i64,
                item_json: serde_json::to_string(item)?,
            })
            .exec(&mut tx)
            .await?;
        }
        delete_setting_in_tx(&mut tx, &session_context_tokens_key(agent_id, session_id)).await?;

        tx.commit().await?;
        Ok(())
    }

    pub async fn save_session_context_tokens(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        tokens: u64,
    ) -> Result<()> {
        self.set_setting(
            &session_context_tokens_key(agent_id, session_id),
            &tokens.to_string(),
        )
        .await
    }

    pub async fn clear_session_context_tokens(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        delete_setting_in_tx(&mut tx, &session_context_tokens_key(agent_id, session_id)).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn append_service_event(&self, event: &ServiceEvent) -> Result<()> {
        let mut db = self.db.clone();
        Query::<List<ServiceEventRecord>>::filter(
            ServiceEventRecord::fields()
                .sequence()
                .eq(u64_to_i64(event.sequence)),
        )
        .delete()
        .exec(&mut db)
        .await?;
        toasty::create!(ServiceEventRecord {
            sequence: u64_to_i64(event.sequence),
            timestamp: event.timestamp.to_rfc3339(),
            agent_id: event_agent_id(event).map(|id| id.to_string()),
            session_id: event_session_id(event).map(|id| id.to_string()),
            event_json: serde_json::to_string(event)?,
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn service_events_after(
        &self,
        sequence: u64,
        limit: usize,
    ) -> Result<Vec<ServiceEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut db = self.db.clone();
        let mut rows = Query::<List<ServiceEventRecord>>::all()
            .exec(&mut db)
            .await?;
        rows.retain(|row| i64_to_u64(row.sequence) > sequence);
        rows.sort_by_key(|row| row.sequence);
        rows.into_iter()
            .take(limit)
            .map(|row| serde_json::from_str::<ServiceEvent>(&row.event_json).map_err(Into::into))
            .collect()
    }

    pub async fn prune_service_events_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        let mut db = self.db.clone();
        let cutoff = cutoff.to_rfc3339();
        let rows = Query::<List<ServiceEventRecord>>::all()
            .exec(&mut db)
            .await?;
        let old_sequences = rows
            .into_iter()
            .filter(|row| row.timestamp < cutoff)
            .map(|row| row.sequence)
            .collect::<Vec<_>>();
        for sequence in &old_sequences {
            Query::<List<ServiceEventRecord>>::filter(
                ServiceEventRecord::fields().sequence().eq(*sequence),
            )
            .delete()
            .exec(&mut db)
            .await?;
        }
        Ok(old_sequences.len())
    }

    pub async fn append_agent_log_entry(&self, entry: &AgentLogEntry) -> Result<()> {
        let mut db = self.db.clone();
        toasty::create!(AgentLogRecord {
            id: entry.id.to_string(),
            agent_id: entry.agent_id.to_string(),
            session_id: entry.session_id.map(|id| id.to_string()),
            turn_id: entry.turn_id.map(|id| id.to_string()),
            level: entry.level.clone(),
            category: entry.category.clone(),
            message: entry.message.clone(),
            details_json: serde_json::to_string(&entry.details)?,
            timestamp: entry.timestamp.to_rfc3339(),
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn list_agent_logs(
        &self,
        agent_id: AgentId,
        filter: AgentLogFilter,
    ) -> Result<Vec<AgentLogEntry>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<AgentLogRecord>>::filter(
            AgentLogRecord::fields().agent_id().eq(agent_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        if let Some(session_id) = filter.session_id {
            let session_id = session_id.to_string();
            rows.retain(|row| row.session_id.as_deref() == Some(session_id.as_str()));
        }
        if let Some(turn_id) = filter.turn_id {
            let turn_id = turn_id.to_string();
            rows.retain(|row| row.turn_id.as_deref() == Some(turn_id.as_str()));
        }
        if let Some(level) = filter.level {
            rows.retain(|row| row.level == level);
        }
        if let Some(category) = filter.category {
            rows.retain(|row| row.category == category);
        }
        if let Some(since) = filter.since {
            let since = since.to_rfc3339();
            rows.retain(|row| row.timestamp >= since);
        }
        if let Some(until) = filter.until {
            let until = until.to_rfc3339();
            rows.retain(|row| row.timestamp <= until);
        }
        rows.sort_by(|left, right| {
            right
                .timestamp
                .cmp(&left.timestamp)
                .then_with(|| right.id.cmp(&left.id))
        });
        rows.into_iter()
            .skip(filter.offset)
            .take(filter.limit.max(1))
            .map(AgentLogRecord::into_entry)
            .collect()
    }

    pub async fn prune_agent_logs_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        let mut db = self.db.clone();
        let cutoff = cutoff.to_rfc3339();
        let rows = Query::<List<AgentLogRecord>>::all().exec(&mut db).await?;
        let old_ids = rows
            .into_iter()
            .filter(|row| row.timestamp < cutoff)
            .map(|row| row.id)
            .collect::<Vec<_>>();
        for id in &old_ids {
            Query::<List<AgentLogRecord>>::filter(AgentLogRecord::fields().id().eq(id.clone()))
                .delete()
                .exec(&mut db)
                .await?;
        }
        Ok(old_ids.len())
    }

    pub async fn save_tool_trace_started(
        &self,
        trace: &ToolTraceDetail,
        started_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut db = self.db.clone();
        delete_matching_tool_trace(&mut db, trace).await?;
        toasty::create!(ToolTraceRecord {
            id: tool_trace_record_id(trace),
            call_id: trace.call_id.clone(),
            agent_id: trace.agent_id.to_string(),
            session_id: trace.session_id.map(|id| id.to_string()),
            turn_id: trace.turn_id.map(|id| id.to_string()),
            tool_name: trace.tool_name.clone(),
            arguments_json: serde_json::to_string(&trace.arguments)?,
            output: String::new(),
            success: false,
            duration_ms: None,
            started_at: started_at.to_rfc3339(),
            completed_at: None,
            output_preview: String::new(),
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn save_tool_trace_completed(
        &self,
        trace: &ToolTraceDetail,
        started_at: DateTime<Utc>,
        completed_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut db = self.db.clone();
        delete_matching_tool_trace(&mut db, trace).await?;
        toasty::create!(ToolTraceRecord {
            id: tool_trace_record_id(trace),
            call_id: trace.call_id.clone(),
            agent_id: trace.agent_id.to_string(),
            session_id: trace.session_id.map(|id| id.to_string()),
            turn_id: trace.turn_id.map(|id| id.to_string()),
            tool_name: trace.tool_name.clone(),
            arguments_json: serde_json::to_string(&trace.arguments)?,
            output: trace.output.clone(),
            success: trace.success,
            duration_ms: trace.duration_ms.map(u64_to_i64),
            started_at: started_at.to_rfc3339(),
            completed_at: Some(completed_at.to_rfc3339()),
            output_preview: trace.output_preview.clone(),
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn load_tool_trace(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        call_id: &str,
    ) -> Result<Option<ToolTraceDetail>> {
        let mut db = self.db.clone();
        let rows = Query::<List<ToolTraceRecord>>::filter(
            ToolTraceRecord::fields().call_id().eq(call_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.into_iter()
            .find(|row| {
                tool_trace_belongs_to(
                    row,
                    agent_id,
                    session_id.map(|id| id.to_string()).as_deref(),
                    None,
                )
            })
            .map(ToolTraceRecord::into_detail)
            .transpose()
    }

    pub async fn list_tool_traces(
        &self,
        agent_id: AgentId,
        filter: ToolTraceFilter,
    ) -> Result<Vec<ToolTraceSummary>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<ToolTraceRecord>>::filter(
            ToolTraceRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        if let Some(session_id) = filter.session_id {
            let session_id = session_id.to_string();
            rows.retain(|row| row.session_id.as_deref() == Some(session_id.as_str()));
        }
        if let Some(turn_id) = filter.turn_id {
            let turn_id = turn_id.to_string();
            rows.retain(|row| row.turn_id.as_deref() == Some(turn_id.as_str()));
        }
        rows.sort_by(|left, right| {
            right
                .started_at
                .cmp(&left.started_at)
                .then_with(|| right.call_id.cmp(&left.call_id))
        });
        rows.into_iter()
            .skip(filter.offset)
            .take(filter.limit.max(1))
            .map(ToolTraceRecord::into_summary)
            .collect()
    }

    pub async fn prune_tool_traces_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        let mut db = self.db.clone();
        let cutoff = cutoff.to_rfc3339();
        let rows = Query::<List<ToolTraceRecord>>::all().exec(&mut db).await?;
        let old_ids = rows
            .into_iter()
            .filter(|row| row.started_at < cutoff)
            .map(|row| row.id)
            .collect::<Vec<_>>();
        for id in &old_ids {
            Query::<List<ToolTraceRecord>>::filter(ToolTraceRecord::fields().id().eq(id.clone()))
                .delete()
                .exec(&mut db)
                .await?;
        }
        Ok(old_ids.len())
    }

    pub async fn load_runtime_snapshot(
        &self,
        recent_event_limit: usize,
    ) -> Result<RuntimeSnapshot> {
        let mut db = self.db.clone();
        let mut agent_rows = Query::<List<AgentRecordRow>>::all().exec(&mut db).await?;
        agent_rows.sort_by(|left, right| left.created_at.cmp(&right.created_at));

        let mut agents = Vec::with_capacity(agent_rows.len());
        for row in agent_rows {
            let agent_id = parse_agent_id(&row.id)?;
            let sessions = self.load_agent_sessions(agent_id).await?;
            let system_prompt = row.system_prompt.clone();
            agents.push(PersistedAgent {
                summary: row.into_summary()?,
                sessions,
                system_prompt,
            });
        }

        let mut task_rows = Query::<List<TaskRecordRow>>::all().exec(&mut db).await?;
        task_rows.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        let mut tasks = Vec::with_capacity(task_rows.len());
        for row in task_rows {
            let task_id = parse_task_id(&row.id)?;
            let reviews = self.load_task_reviews(task_id).await?;
            let plan_history = self.load_plan_history(task_id).await?;
            tasks.push(row.into_persisted_task(reviews, plan_history)?);
        }
        let projects = self.load_projects().await?;

        let mut events = Query::<List<ServiceEventRecord>>::all()
            .exec(&mut db)
            .await?;
        events.sort_by_key(|event| event.sequence);
        let next_sequence = events
            .last()
            .map(|event| i64_to_u64(event.sequence).saturating_add(1))
            .unwrap_or(1);
        let skip = events.len().saturating_sub(recent_event_limit);
        let recent_events = events
            .into_iter()
            .skip(skip)
            .map(|row| serde_json::from_str::<ServiceEvent>(&row.event_json).map_err(Into::into))
            .collect::<Result<Vec<_>>>()?;

        Ok(RuntimeSnapshot {
            agents,
            tasks,
            projects,
            recent_events,
            next_sequence,
        })
    }

    async fn load_agent_sessions(&self, agent_id: AgentId) -> Result<Vec<PersistedAgentSession>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<AgentSessionRecord>>::filter(
            AgentSessionRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });

        let mut sessions = Vec::with_capacity(rows.len());
        for row in rows {
            let session_id = parse_session_id(&row.id)?;
            let messages = self.load_agent_messages(agent_id, session_id).await?;
            let history = self.load_agent_history(agent_id, session_id).await?;
            let last_context_tokens = self
                .load_session_context_tokens(agent_id, session_id)
                .await?;
            sessions.push(PersistedAgentSession {
                summary: AgentSessionSummary {
                    id: session_id,
                    title: row.title,
                    created_at: parse_utc(&row.created_at)?,
                    updated_at: parse_utc(&row.updated_at)?,
                    message_count: messages.len(),
                },
                history,
                last_context_tokens,
            });
        }
        Ok(sessions)
    }

    pub async fn load_agent_messages(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<Vec<AgentMessage>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<AgentMessageRecord>>::filter(
            AgentMessageRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.retain(|row| row.session_id == session_id.to_string());
        rows.sort_by_key(|row| row.position);
        rows.into_iter()
            .map(AgentMessageRecord::into_message)
            .collect()
    }

    async fn load_agent_history(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<Vec<ModelInputItem>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<AgentHistoryRecord>>::filter(
            AgentHistoryRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.retain(|row| row.session_id == session_id.to_string());
        rows.sort_by_key(|row| row.position);
        rows.into_iter()
            .map(|row| serde_json::from_str::<ModelInputItem>(&row.item_json).map_err(Into::into))
            .collect()
    }

    async fn load_session_context_tokens(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<Option<u64>> {
        Ok(self
            .get_setting(&session_context_tokens_key(agent_id, session_id))
            .await?
            .and_then(|value| value.parse::<u64>().ok()))
    }

    pub async fn load_task_reviews(&self, task_id: TaskId) -> Result<Vec<TaskReview>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<TaskReviewRecord>>::filter(
            TaskReviewRecord::fields().task_id().eq(task_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.sort_by(|left, right| {
            left.round
                .cmp(&right.round)
                .then_with(|| left.created_at.cmp(&right.created_at))
        });
        rows.into_iter()
            .map(TaskReviewRecord::into_review)
            .collect()
    }
}

impl AgentRecordRow {
    fn into_summary(self) -> Result<AgentSummary> {
        Ok(AgentSummary {
            id: parse_agent_id(&self.id)?,
            parent_id: self.parent_id.as_deref().map(parse_agent_id).transpose()?,
            task_id: self.task_id.as_deref().map(parse_task_id).transpose()?,
            project_id: self
                .project_id
                .as_deref()
                .map(parse_project_id)
                .transpose()?,
            role: self.role.as_deref().map(parse_store_enum).transpose()?,
            name: self.name,
            status: parse_store_enum(&self.status)?,
            container_id: self.container_id,
            docker_image: self.docker_image,
            provider_id: self.provider_id,
            provider_name: self.provider_name,
            model: self.model,
            reasoning_effort: self.reasoning_effort,
            created_at: parse_utc(&self.created_at)?,
            updated_at: parse_utc(&self.updated_at)?,
            current_turn: self
                .current_turn
                .as_deref()
                .map(parse_turn_id)
                .transpose()?,
            last_error: self.last_error,
            token_usage: TokenUsage {
                input_tokens: i64_to_u64(self.input_tokens),
                output_tokens: i64_to_u64(self.output_tokens),
                total_tokens: i64_to_u64(self.total_tokens),
            },
        })
    }
}

impl ProjectRecordRow {
    fn into_summary(self) -> Result<ProjectSummary> {
        Ok(ProjectSummary {
            id: parse_project_id(&self.id)?,
            name: self.name,
            status: parse_store_enum(&self.status)?,
            owner: self.owner,
            repo: self.repo,
            repository_full_name: self.repository_full_name,
            git_account_id: self.git_account_id,
            repository_id: i64_to_u64(self.repository_id),
            installation_id: i64_to_u64(self.installation_id),
            installation_account: self.installation_account,
            branch: self.branch,
            docker_image: self.docker_image,
            clone_status: parse_store_enum(&self.clone_status)?,
            maintainer_agent_id: parse_agent_id(&self.maintainer_agent_id)?,
            created_at: parse_utc(&self.created_at)?,
            updated_at: parse_utc(&self.updated_at)?,
            last_error: self.last_error,
            auto_review_enabled: self.auto_review_enabled,
            reviewer_extra_prompt: self.reviewer_extra_prompt,
            review_status: parse_store_enum(&self.review_status)?,
            current_reviewer_agent_id: self
                .current_reviewer_agent_id
                .as_deref()
                .map(parse_agent_id)
                .transpose()?,
            last_review_started_at: self
                .last_review_started_at
                .as_deref()
                .map(parse_utc)
                .transpose()?,
            last_review_finished_at: self
                .last_review_finished_at
                .as_deref()
                .map(parse_utc)
                .transpose()?,
            next_review_at: self.next_review_at.as_deref().map(parse_utc).transpose()?,
            last_review_outcome: self
                .last_review_outcome
                .as_deref()
                .map(parse_store_enum)
                .transpose()?,
            review_last_error: self.review_last_error,
        })
    }
}

impl StoredGitAccount {
    fn summary(&self, default_account_id: Option<&str>) -> GitAccountSummary {
        GitAccountSummary {
            id: self.id.clone(),
            provider: self.provider.clone(),
            label: self.label.clone(),
            login: self.login.clone(),
            token_kind: self.token_kind.clone(),
            scopes: self.scopes.clone(),
            status: self.status.clone(),
            is_default: default_account_id == Some(self.id.as_str()),
            has_token: !self.token_secret.trim().is_empty(),
            last_verified_at: self.last_verified_at,
            last_error: self.last_error.clone(),
        }
    }
}

fn git_accounts_response(config: &GitAccountsConfig) -> GitAccountsResponse {
    GitAccountsResponse {
        accounts: config
            .accounts
            .iter()
            .map(|account| account.summary(config.default_account_id.as_deref()))
            .collect(),
        default_account_id: config.default_account_id.clone(),
    }
}

fn normalize_git_account_defaults(config: &mut GitAccountsConfig) {
    if config.default_account_id.is_none() {
        config.default_account_id = config.accounts.first().map(|account| account.id.clone());
    }
    let default_account_id = config.default_account_id.clone();
    for account in &mut config.accounts {
        account.is_default = default_account_id.as_deref() == Some(account.id.as_str());
    }
}

impl TaskRecordRow {
    fn into_persisted_task(
        self,
        reviews: Vec<TaskReview>,
        plan_history: Vec<PlanHistoryEntry>,
    ) -> Result<PersistedTask> {
        let plan = TaskPlan {
            status: parse_store_enum(&self.plan_status)?,
            title: self.plan_title,
            markdown: self.plan_markdown,
            version: i64_to_u64(self.plan_version),
            saved_by_agent_id: self
                .plan_saved_by_agent_id
                .as_deref()
                .map(parse_agent_id)
                .transpose()?,
            saved_at: self.plan_saved_at.as_deref().map(parse_utc).transpose()?,
            approved_at: self
                .plan_approved_at
                .as_deref()
                .map(parse_utc)
                .transpose()?,
            revision_feedback: self.plan_revision_feedback,
            revision_requested_at: self
                .plan_revision_requested_at
                .as_deref()
                .map(parse_utc)
                .transpose()?,
        };
        let summary = TaskSummary {
            id: parse_task_id(&self.id)?,
            title: self.title,
            status: parse_store_enum(&self.status)?,
            plan_status: plan.status.clone(),
            plan_version: plan.version,
            planner_agent_id: parse_agent_id(&self.planner_agent_id)?,
            current_agent_id: self
                .current_agent_id
                .as_deref()
                .map(parse_agent_id)
                .transpose()?,
            agent_count: 0,
            review_rounds: reviews.len() as u64,
            created_at: parse_utc(&self.created_at)?,
            updated_at: parse_utc(&self.updated_at)?,
            last_error: self.last_error,
            final_report: self.final_report,
        };
        Ok(PersistedTask {
            summary,
            plan,
            plan_history,
            reviews,
            artifacts: Vec::new(),
        })
    }
}

impl AgentMessageRecord {
    fn into_message(self) -> Result<AgentMessage> {
        Ok(AgentMessage {
            role: parse_store_enum(&self.role)?,
            content: self.content,
            created_at: parse_utc(&self.created_at)?,
        })
    }
}

impl TaskReviewRecord {
    fn into_review(self) -> Result<TaskReview> {
        Ok(TaskReview {
            id: parse_task_id(&self.id)?,
            task_id: parse_task_id(&self.task_id)?,
            reviewer_agent_id: parse_agent_id(&self.reviewer_agent_id)?,
            round: i64_to_u64(self.round),
            passed: self.passed,
            findings: self.findings,
            summary: self.summary,
            created_at: parse_utc(&self.created_at)?,
        })
    }
}

impl ProjectReviewRunRecord {
    fn into_summary(self) -> Result<ProjectReviewRunSummary> {
        Ok(ProjectReviewRunSummary {
            id: parse_uuid(&self.id)?,
            project_id: parse_project_id(&self.project_id)?,
            reviewer_agent_id: self
                .reviewer_agent_id
                .as_deref()
                .map(parse_agent_id)
                .transpose()?,
            turn_id: self.turn_id.as_deref().map(parse_turn_id).transpose()?,
            started_at: parse_utc(&self.started_at)?,
            finished_at: self.finished_at.as_deref().map(parse_utc).transpose()?,
            status: parse_store_enum(&self.status)?,
            outcome: self.outcome.as_deref().map(parse_store_enum).transpose()?,
            pr: self.pr.map(i64_to_u64),
            summary: self.summary,
            error: self.error,
        })
    }

    fn into_detail(self) -> Result<ProjectReviewRunDetail> {
        let messages = serde_json::from_str::<Vec<AgentMessage>>(&self.messages_json)?;
        let events = serde_json::from_str::<Vec<ServiceEvent>>(&self.events_json)?;
        Ok(ProjectReviewRunDetail {
            summary: self.into_summary()?,
            messages,
            events,
        })
    }
}

impl AgentLogRecord {
    fn into_entry(self) -> Result<AgentLogEntry> {
        Ok(AgentLogEntry {
            id: parse_uuid(&self.id)?,
            agent_id: parse_agent_id(&self.agent_id)?,
            session_id: self
                .session_id
                .as_deref()
                .map(parse_session_id)
                .transpose()?,
            turn_id: self.turn_id.as_deref().map(parse_turn_id).transpose()?,
            level: self.level,
            category: self.category,
            message: self.message,
            details: serde_json::from_str(&self.details_json)?,
            timestamp: parse_utc(&self.timestamp)?,
        })
    }
}

impl ToolTraceRecord {
    fn into_summary(self) -> Result<ToolTraceSummary> {
        Ok(ToolTraceSummary {
            call_id: self.call_id,
            agent_id: parse_agent_id(&self.agent_id)?,
            session_id: self
                .session_id
                .as_deref()
                .map(parse_session_id)
                .transpose()?,
            turn_id: self.turn_id.as_deref().map(parse_turn_id).transpose()?,
            tool_name: self.tool_name,
            success: self.success,
            started_at: parse_utc(&self.started_at)?,
            completed_at: self.completed_at.as_deref().map(parse_utc).transpose()?,
            duration_ms: self.duration_ms.map(i64_to_u64),
            output_preview: self.output_preview,
        })
    }

    fn into_detail(self) -> Result<ToolTraceDetail> {
        Ok(ToolTraceDetail {
            agent_id: parse_agent_id(&self.agent_id)?,
            session_id: self
                .session_id
                .as_deref()
                .map(parse_session_id)
                .transpose()?,
            turn_id: self.turn_id.as_deref().map(parse_turn_id).transpose()?,
            call_id: self.call_id,
            tool_name: self.tool_name,
            arguments: serde_json::from_str(&self.arguments_json)?,
            output: self.output,
            success: self.success,
            duration_ms: self.duration_ms.map(i64_to_u64),
            started_at: Some(parse_utc(&self.started_at)?),
            completed_at: self.completed_at.as_deref().map(parse_utc).transpose()?,
            output_preview: self.output_preview,
        })
    }
}

async fn delete_matching_tool_trace(db: &mut Db, trace: &ToolTraceDetail) -> Result<()> {
    let rows = Query::<List<ToolTraceRecord>>::filter(
        ToolTraceRecord::fields()
            .call_id()
            .eq(trace.call_id.clone()),
    )
    .exec(db)
    .await?;
    let session_id = trace.session_id.map(|id| id.to_string());
    let turn_id = trace.turn_id.map(|id| id.to_string());
    for row in rows {
        if tool_trace_belongs_to(
            &row,
            trace.agent_id,
            session_id.as_deref(),
            turn_id.as_deref(),
        ) {
            Query::<List<ToolTraceRecord>>::filter(ToolTraceRecord::fields().id().eq(row.id))
                .delete()
                .exec(db)
                .await?;
        }
    }
    Ok(())
}

fn tool_trace_record_id(trace: &ToolTraceDetail) -> String {
    format!(
        "{}:{}:{}:{}",
        trace.agent_id,
        trace
            .session_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        trace.turn_id.map(|id| id.to_string()).unwrap_or_default(),
        trace.call_id
    )
}

fn tool_trace_belongs_to(
    row: &ToolTraceRecord,
    agent_id: AgentId,
    session_id: Option<&str>,
    turn_id: Option<&str>,
) -> bool {
    row.agent_id == agent_id.to_string()
        && session_id.is_none_or(|id| row.session_id.as_deref() == Some(id))
        && turn_id.is_none_or(|id| row.turn_id.as_deref() == Some(id))
}

async fn build_db(path: &Path) -> Result<Db> {
    let mut builder = Db::builder();
    builder.models(toasty::models!(
        McpServerRecord,
        SettingRecord,
        ProjectRecordRow,
        TaskRecordRow,
        TaskReviewRecord,
        ProjectReviewRunRecord,
        PlanHistoryRecord,
        AgentRecordRow,
        AgentSessionRecord,
        AgentMessageRecord,
        AgentHistoryRecord,
        ServiceEventRecord,
        AgentLogRecord,
        ToolTraceRecord,
    ));
    builder.max_pool_size(1);
    Ok(builder.build(Sqlite::open(path)).await?)
}

fn migrate_to_v16(path: &Path) -> Result<()> {
    let conn = SqliteConnection::open(path)?;
    if !sqlite_column_exists(&conn, "agents", "project_id")? {
        conn.execute("ALTER TABLE agents ADD COLUMN project_id TEXT", [])?;
    }
    conn.execute(
        "CREATE TABLE IF NOT EXISTS projects (
            id TEXT PRIMARY KEY NOT NULL,
            name TEXT NOT NULL,
            status TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            repository_full_name TEXT NOT NULL DEFAULT '',
            git_account_id TEXT,
            repository_id BIGINT NOT NULL,
            installation_id BIGINT NOT NULL,
            installation_account TEXT NOT NULL,
            branch TEXT NOT NULL DEFAULT '',
            docker_image TEXT NOT NULL,
            clone_status TEXT NOT NULL,
            maintainer_agent_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            last_error TEXT,
            auto_review_enabled BOOLEAN NOT NULL DEFAULT 0,
            reviewer_extra_prompt TEXT,
            review_status TEXT NOT NULL DEFAULT 'disabled',
            current_reviewer_agent_id TEXT,
            last_review_started_at TEXT,
            last_review_finished_at TEXT,
            next_review_at TEXT,
            last_review_outcome TEXT,
            review_last_error TEXT
        )",
        [],
    )?;
    if !sqlite_column_exists(&conn, "projects", "repository_full_name")? {
        conn.execute(
            "ALTER TABLE projects ADD COLUMN repository_full_name TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    if !sqlite_column_exists(&conn, "projects", "git_account_id")? {
        conn.execute("ALTER TABLE projects ADD COLUMN git_account_id TEXT", [])?;
    }
    if !sqlite_column_exists(&conn, "projects", "branch")? {
        conn.execute(
            "ALTER TABLE projects ADD COLUMN branch TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    ensure_project_review_columns(&conn)?;
    conn.execute(
        "UPDATE projects
            SET repository_full_name = owner || '/' || repo
          WHERE repository_full_name = ''",
        [],
    )?;
    drop_project_path_columns(&conn)?;
    ensure_project_review_runs_table(&conn)?;
    ensure_agent_log_tables(&conn)?;
    Ok(())
}

fn drop_project_path_columns(conn: &SqliteConnection) -> Result<()> {
    if !sqlite_column_exists(conn, "projects", "project_path")?
        && !sqlite_column_exists(conn, "projects", "workspace_path")?
    {
        return Ok(());
    }
    conn.execute(
        "CREATE TABLE projects_v12 (
            id TEXT PRIMARY KEY NOT NULL,
            name TEXT NOT NULL,
            status TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            repository_full_name TEXT NOT NULL DEFAULT '',
            git_account_id TEXT,
            repository_id BIGINT NOT NULL,
            installation_id BIGINT NOT NULL,
            installation_account TEXT NOT NULL,
            branch TEXT NOT NULL DEFAULT '',
            docker_image TEXT NOT NULL,
            clone_status TEXT NOT NULL,
            maintainer_agent_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            last_error TEXT,
            auto_review_enabled BOOLEAN NOT NULL DEFAULT 0,
            reviewer_extra_prompt TEXT,
            review_status TEXT NOT NULL DEFAULT 'disabled',
            current_reviewer_agent_id TEXT,
            last_review_started_at TEXT,
            last_review_finished_at TEXT,
            next_review_at TEXT,
            last_review_outcome TEXT,
            review_last_error TEXT
        )",
        [],
    )?;
    conn.execute(
        "INSERT INTO projects_v12 (
            id,
            name,
            status,
            owner,
            repo,
            repository_full_name,
            git_account_id,
            repository_id,
            installation_id,
            installation_account,
            branch,
            docker_image,
            clone_status,
            maintainer_agent_id,
            created_at,
            updated_at,
            last_error,
            auto_review_enabled,
            reviewer_extra_prompt,
            review_status,
            current_reviewer_agent_id,
            last_review_started_at,
            last_review_finished_at,
            next_review_at,
            last_review_outcome,
            review_last_error
        )
        SELECT
            id,
            name,
            status,
            owner,
            repo,
            repository_full_name,
            git_account_id,
            repository_id,
            installation_id,
            installation_account,
            branch,
            docker_image,
            clone_status,
            maintainer_agent_id,
            created_at,
            updated_at,
            last_error,
            auto_review_enabled,
            reviewer_extra_prompt,
            review_status,
            current_reviewer_agent_id,
            last_review_started_at,
            last_review_finished_at,
            next_review_at,
            last_review_outcome,
            review_last_error
        FROM projects",
        [],
    )?;
    conn.execute("DROP TABLE projects", [])?;
    conn.execute("ALTER TABLE projects_v12 RENAME TO projects", [])?;
    Ok(())
}

fn ensure_project_review_columns(conn: &SqliteConnection) -> Result<()> {
    let columns = [
        (
            "auto_review_enabled",
            "ALTER TABLE projects ADD COLUMN auto_review_enabled BOOLEAN NOT NULL DEFAULT 0",
        ),
        (
            "reviewer_extra_prompt",
            "ALTER TABLE projects ADD COLUMN reviewer_extra_prompt TEXT",
        ),
        (
            "review_status",
            "ALTER TABLE projects ADD COLUMN review_status TEXT NOT NULL DEFAULT 'disabled'",
        ),
        (
            "current_reviewer_agent_id",
            "ALTER TABLE projects ADD COLUMN current_reviewer_agent_id TEXT",
        ),
        (
            "last_review_started_at",
            "ALTER TABLE projects ADD COLUMN last_review_started_at TEXT",
        ),
        (
            "last_review_finished_at",
            "ALTER TABLE projects ADD COLUMN last_review_finished_at TEXT",
        ),
        (
            "next_review_at",
            "ALTER TABLE projects ADD COLUMN next_review_at TEXT",
        ),
        (
            "last_review_outcome",
            "ALTER TABLE projects ADD COLUMN last_review_outcome TEXT",
        ),
        (
            "review_last_error",
            "ALTER TABLE projects ADD COLUMN review_last_error TEXT",
        ),
    ];
    for (column, statement) in columns {
        if !sqlite_column_exists(conn, "projects", column)? {
            conn.execute(statement, [])?;
        }
    }
    Ok(())
}

fn ensure_project_review_runs_table(conn: &SqliteConnection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS project_review_runs (
            id TEXT PRIMARY KEY NOT NULL,
            project_id TEXT NOT NULL,
            reviewer_agent_id TEXT,
            turn_id TEXT,
            started_at TEXT NOT NULL,
            finished_at TEXT,
            status TEXT NOT NULL,
            outcome TEXT,
            pr BIGINT,
            summary TEXT,
            error TEXT,
            messages_json TEXT NOT NULL DEFAULT '[]',
            events_json TEXT NOT NULL DEFAULT '[]'
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_project_review_runs_project_id
            ON project_review_runs(project_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_project_review_runs_started_at
            ON project_review_runs(started_at)",
        [],
    )?;
    Ok(())
}

fn ensure_agent_log_tables(conn: &SqliteConnection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS agent_log_entries (
            id TEXT PRIMARY KEY NOT NULL,
            agent_id TEXT NOT NULL,
            session_id TEXT,
            turn_id TEXT,
            level TEXT NOT NULL,
            category TEXT NOT NULL,
            message TEXT NOT NULL,
            details_json TEXT NOT NULL DEFAULT '{}',
            timestamp TEXT NOT NULL
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_agent_log_entries_agent_id
            ON agent_log_entries(agent_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_agent_log_entries_session_id
            ON agent_log_entries(session_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_agent_log_entries_turn_id
            ON agent_log_entries(turn_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_agent_log_entries_category
            ON agent_log_entries(category)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_agent_log_entries_timestamp
            ON agent_log_entries(timestamp)",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS tool_trace_records (
            id TEXT PRIMARY KEY NOT NULL,
            call_id TEXT NOT NULL,
            agent_id TEXT NOT NULL,
            session_id TEXT,
            turn_id TEXT,
            tool_name TEXT NOT NULL,
            arguments_json TEXT NOT NULL DEFAULT '{}',
            output TEXT NOT NULL DEFAULT '',
            success BOOLEAN NOT NULL DEFAULT 0,
            duration_ms BIGINT,
            started_at TEXT NOT NULL,
            completed_at TEXT,
            output_preview TEXT NOT NULL DEFAULT ''
        )",
        [],
    )?;
    ensure_tool_trace_id_column(conn)?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tool_trace_records_call_id
            ON tool_trace_records(call_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tool_trace_records_agent_id
            ON tool_trace_records(agent_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tool_trace_records_session_id
            ON tool_trace_records(session_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tool_trace_records_turn_id
            ON tool_trace_records(turn_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tool_trace_records_started_at
            ON tool_trace_records(started_at)",
        [],
    )?;
    Ok(())
}

fn ensure_tool_trace_id_column(conn: &SqliteConnection) -> Result<()> {
    if !sqlite_table_exists(conn, "tool_trace_records")?
        || sqlite_column_exists(conn, "tool_trace_records", "id")?
    {
        return Ok(());
    }

    conn.execute(
        "ALTER TABLE tool_trace_records RENAME TO tool_trace_records_v15",
        [],
    )?;
    conn.execute(
        "CREATE TABLE tool_trace_records (
            id TEXT PRIMARY KEY NOT NULL,
            call_id TEXT NOT NULL,
            agent_id TEXT NOT NULL,
            session_id TEXT,
            turn_id TEXT,
            tool_name TEXT NOT NULL,
            arguments_json TEXT NOT NULL DEFAULT '{}',
            output TEXT NOT NULL DEFAULT '',
            success BOOLEAN NOT NULL DEFAULT 0,
            duration_ms BIGINT,
            started_at TEXT NOT NULL,
            completed_at TEXT,
            output_preview TEXT NOT NULL DEFAULT ''
        )",
        [],
    )?;
    conn.execute(
        "INSERT INTO tool_trace_records (
            id,
            call_id,
            agent_id,
            session_id,
            turn_id,
            tool_name,
            arguments_json,
            output,
            success,
            duration_ms,
            started_at,
            completed_at,
            output_preview
        )
        SELECT
            agent_id || ':' || COALESCE(session_id, '') || ':' || COALESCE(turn_id, '') || ':' || call_id,
            call_id,
            agent_id,
            session_id,
            turn_id,
            tool_name,
            arguments_json,
            output,
            success,
            duration_ms,
            started_at,
            completed_at,
            output_preview
        FROM tool_trace_records_v15",
        [],
    )?;
    conn.execute("DROP TABLE tool_trace_records_v15", [])?;
    Ok(())
}

fn sqlite_column_exists(conn: &SqliteConnection, table: &str, column: &str) -> Result<bool> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn sqlite_table_exists(conn: &SqliteConnection, table: &str) -> Result<bool> {
    let mut statement =
        conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1")?;
    let mut rows = statement.query([table])?;
    Ok(rows.next()?.is_some())
}

trait StringDefault {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str;
}

impl StringDefault for str {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str {
        if self.is_empty() { fallback } else { self }
    }
}

fn has_sqlite_header(path: &Path) -> Result<bool> {
    let mut header = [0_u8; 16];
    let bytes_read = std::io::Read::read(&mut std::fs::File::open(path)?, &mut header)?;
    Ok(bytes_read == SQLITE_HEADER.len() && header.as_slice() == SQLITE_HEADER)
}

async fn get_setting_on(db: &Db, key: &str) -> Result<Option<String>> {
    let mut db = db.clone();
    let row =
        Query::<List<SettingRecord>>::filter(SettingRecord::fields().key().eq(key.to_string()))
            .first()
            .exec(&mut db)
            .await?;
    Ok(row.map(|row| row.value))
}

async fn set_setting_on(db: &mut Db, key: &str, value: &str) -> Result<()> {
    let mut tx = db.transaction().await?;
    set_setting_in_tx(&mut tx, key, value).await?;
    tx.commit().await?;
    Ok(())
}

async fn set_setting_in_tx(tx: &mut toasty::Transaction<'_>, key: &str, value: &str) -> Result<()> {
    delete_setting_in_tx(tx, key).await?;
    toasty::create!(SettingRecord {
        key: key.to_string(),
        value: value.to_string(),
    })
    .exec(tx)
    .await?;
    Ok(())
}

async fn delete_setting_in_tx(tx: &mut toasty::Transaction<'_>, key: &str) -> Result<()> {
    Query::<List<SettingRecord>>::filter(SettingRecord::fields().key().eq(key.to_string()))
        .delete()
        .exec(tx)
        .await?;
    Ok(())
}

async fn delete_agent_row_in_tx(tx: &mut toasty::Transaction<'_>, agent_id: AgentId) -> Result<()> {
    Query::<List<AgentRecordRow>>::filter(AgentRecordRow::fields().id().eq(agent_id.to_string()))
        .delete()
        .exec(tx)
        .await?;
    Ok(())
}

impl ModelToml {
    fn from_model(model: ModelConfig) -> Self {
        Self {
            name: model.name,
            context_tokens: model.context_tokens,
            output_tokens: model.output_tokens,
            supports_tools: model.supports_tools,
            wire_api: model.wire_api,
            capabilities: model.capabilities,
            request_policy: model.request_policy,
            reasoning: model.reasoning,
            options: model.options,
            headers: model.headers,
        }
    }

    fn into_model(self, id: String, provider_kind: ProviderKind) -> ModelConfig {
        let wire_api = migrated_wire_api(provider_kind, self.wire_api);
        let capabilities = migrated_capabilities(provider_kind, &id, self.capabilities);
        let request_policy = migrated_request_policy(provider_kind, self.request_policy);
        ModelConfig {
            id,
            name: self.name,
            context_tokens: self.context_tokens,
            output_tokens: self.output_tokens,
            supports_tools: self.supports_tools,
            wire_api,
            capabilities,
            request_policy,
            reasoning: self.reasoning,
            options: self.options,
            headers: self.headers,
        }
    }
}

fn providers_cache_stamp(path: &Path) -> Result<ProvidersCacheStamp> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(ProvidersCacheStamp {
            exists: true,
            modified: metadata.modified().ok(),
            len: metadata.len(),
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ProvidersCacheStamp {
            exists: false,
            modified: None,
            len: 0,
        }),
        Err(err) => Err(err.into()),
    }
}

fn migrated_wire_api(provider_kind: ProviderKind, wire_api: ModelWireApi) -> ModelWireApi {
    match provider_kind {
        ProviderKind::Openai => wire_api,
        ProviderKind::Deepseek | ProviderKind::Mimo => ModelWireApi::ChatCompletions,
    }
}

fn migrated_capabilities(
    provider_kind: ProviderKind,
    model_id: &str,
    mut capabilities: ModelCapabilities,
) -> ModelCapabilities {
    match provider_kind {
        ProviderKind::Openai => {
            capabilities.continuation = true;
            capabilities.parallel_tools = true;
        }
        ProviderKind::Deepseek => {
            capabilities.continuation = false;
            capabilities.reasoning_replay = capabilities.reasoning_replay
                || model_id.contains("reasoner")
                || model_id.contains("pro");
        }
        ProviderKind::Mimo => {
            capabilities.continuation = false;
        }
    }
    capabilities
}

fn migrated_request_policy(
    provider_kind: ProviderKind,
    mut request_policy: ModelRequestPolicy,
) -> ModelRequestPolicy {
    match provider_kind {
        ProviderKind::Openai => {
            request_policy.store = request_policy.store.or(Some(true));
            if request_policy.max_tokens_field == "max_tokens" {
                request_policy.max_tokens_field = "max_output_tokens".to_string();
            }
        }
        ProviderKind::Deepseek | ProviderKind::Mimo => {
            if request_policy.store == Some(true) {
                request_policy.store = None;
            }
            if request_policy.max_tokens_field.trim().is_empty() {
                request_policy.max_tokens_field = "max_tokens".to_string();
            }
        }
    }
    request_policy
}

fn provider_preset(kind: ProviderKind) -> ProviderPreset {
    let provider = builtin_provider(kind);
    provider_preset_from_config(provider)
}

fn provider_preset_from_config(provider: ProviderConfig) -> ProviderPreset {
    ProviderPreset {
        id: provider.id,
        kind: provider.kind,
        name: provider.name,
        base_url: provider.base_url,
        default_model: provider.default_model,
        models: provider.models,
    }
}

fn builtin_provider(kind: ProviderKind) -> ProviderConfig {
    match kind {
        ProviderKind::Openai => ProviderConfig {
            id: "openai".to_string(),
            kind,
            name: "OpenAI".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: None,
            api_key_env: Some("OPENAI_API_KEY".to_string()),
            default_model: "gpt-5.5".to_string(),
            enabled: true,
            models: vec![
                openai_reasoning_model("gpt-5.5", 400_000, 128_000),
                openai_reasoning_model("gpt-5.4", 400_000, 128_000),
                openai_reasoning_model("gpt-5.4-mini", 400_000, 128_000),
                openai_reasoning_model("gpt-5.4-nano", 400_000, 128_000),
                openai_reasoning_model("gpt-5", 400_000, 128_000),
                ModelConfig {
                    id: "gpt-4.1".to_string(),
                    name: Some("GPT-4.1".to_string()),
                    context_tokens: 1_047_576,
                    output_tokens: 32_768,
                    supports_tools: true,
                    wire_api: ModelWireApi::Responses,
                    capabilities: openai_capabilities(),
                    request_policy: openai_request_policy(),
                    reasoning: None,
                    options: serde_json::Value::Null,
                    headers: BTreeMap::new(),
                },
            ],
        },
        ProviderKind::Deepseek => ProviderConfig {
            id: "deepseek".to_string(),
            kind,
            name: "DeepSeek".to_string(),
            base_url: "https://api.deepseek.com".to_string(),
            api_key: None,
            api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
            default_model: "deepseek-v4-flash".to_string(),
            enabled: true,
            models: vec![
                deepseek_model("deepseek-v4-flash", false),
                deepseek_model("deepseek-v4-pro", true),
                deepseek_model("deepseek-chat", false),
                deepseek_model("deepseek-reasoner", true),
            ],
        },
        ProviderKind::Mimo => mimo_builtin_provider(
            "mimo-api",
            "MiMo API",
            "https://api.xiaomimimo.com/v1",
            "MIMO_API_KEY",
        ),
    }
}

fn mimo_builtin_provider(
    id: &str,
    name: &str,
    base_url: &str,
    api_key_env: &str,
) -> ProviderConfig {
    ProviderConfig {
        id: id.to_string(),
        kind: ProviderKind::Mimo,
        name: name.to_string(),
        base_url: base_url.to_string(),
        api_key: None,
        api_key_env: Some(api_key_env.to_string()),
        default_model: "mimo-v2.5-pro".to_string(),
        enabled: true,
        models: vec![
            mimo_model("mimo-v2.5-pro", true),
            mimo_model("mimo-v2.5", true),
            mimo_model("mimo-v2-pro", true),
            mimo_model("mimo-v2-omni", true),
            mimo_model("mimo-v2-flash", false),
        ],
    }
}

fn openai_reasoning_model(id: &str, context_tokens: u64, output_tokens: u64) -> ModelConfig {
    let mut variants = vec!["minimal", "low", "medium", "high"];
    if id.contains("5.4") || id.contains("5.5") {
        variants.push("xhigh");
    }
    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens,
        output_tokens,
        supports_tools: true,
        wire_api: ModelWireApi::Responses,
        capabilities: openai_capabilities(),
        request_policy: openai_request_policy(),
        reasoning: Some(openai_reasoning_config(variants, "medium")),
        options: serde_json::Value::Null,
        headers: BTreeMap::new(),
    }
}

fn deepseek_model(id: &str, with_reasoning: bool) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens: deepseek_context_tokens(id),
        output_tokens: deepseek_output_tokens(id),
        supports_tools: true,
        wire_api: ModelWireApi::ChatCompletions,
        capabilities: deepseek_capabilities(with_reasoning),
        request_policy: chat_request_policy("max_tokens"),
        reasoning: with_reasoning.then(|| deepseek_reasoning_config(vec!["high", "max"], "high")),
        options: serde_json::Value::Null,
        headers: BTreeMap::new(),
    }
}

fn openai_reasoning_config(variants: Vec<&str>, default_variant: &str) -> ModelReasoningConfig {
    ModelReasoningConfig {
        default_variant: Some(default_variant.to_string()),
        variants: variants
            .into_iter()
            .map(|id| ModelReasoningVariant {
                id: id.to_string(),
                label: None,
                request: serde_json::json!({
                    "reasoning": {
                        "effort": id,
                    },
                }),
            })
            .collect(),
    }
}

fn deepseek_reasoning_config(variants: Vec<&str>, default_variant: &str) -> ModelReasoningConfig {
    ModelReasoningConfig {
        default_variant: Some(default_variant.to_string()),
        variants: variants
            .into_iter()
            .map(|id| ModelReasoningVariant {
                id: id.to_string(),
                label: None,
                request: serde_json::json!({
                    "thinking": {
                        "type": "enabled",
                    },
                    "reasoning_effort": id,
                }),
            })
            .collect(),
    }
}

fn deepseek_context_tokens(id: &str) -> u64 {
    if is_deepseek_v4_model(id) {
        DEEPSEEK_V4_CONTEXT_TOKENS
    } else {
        128_000
    }
}

fn deepseek_output_tokens(id: &str) -> u64 {
    if is_deepseek_v4_model(id) {
        DEEPSEEK_V4_OUTPUT_TOKENS
    } else {
        8_192
    }
}

fn is_deepseek_v4_model(id: &str) -> bool {
    matches!(
        id,
        "deepseek-v4-flash" | "deepseek-v4-pro" | "deepseek-chat" | "deepseek-reasoner"
    )
}

fn mimo_model(id: &str, with_reasoning: bool) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens: mimo_context_tokens(id),
        output_tokens: mimo_output_tokens(id),
        supports_tools: true,
        wire_api: ModelWireApi::ChatCompletions,
        capabilities: mimo_capabilities(with_reasoning),
        request_policy: chat_request_policy("max_tokens"),
        reasoning: with_reasoning.then(mimo_reasoning_config),
        options: serde_json::Value::Null,
        headers: BTreeMap::new(),
    }
}

fn mimo_context_tokens(id: &str) -> u64 {
    match id {
        "mimo-v2.5-pro" | "mimo-v2-pro" | "mimo-v2.5" => 1_000_000,
        "mimo-v2-omni" | "mimo-v2-flash" => 256_000,
        _ => 128_000,
    }
}

fn mimo_output_tokens(id: &str) -> u64 {
    match id {
        "mimo-v2.5-pro" | "mimo-v2-pro" | "mimo-v2.5" | "mimo-v2-omni" => 131_072,
        "mimo-v2-flash" => 65_536,
        _ => 32_768,
    }
}

fn mimo_reasoning_config() -> ModelReasoningConfig {
    ModelReasoningConfig {
        default_variant: Some("high".to_string()),
        variants: vec![ModelReasoningVariant {
            id: "high".to_string(),
            label: None,
            request: serde_json::json!({
                "thinking": {
                    "type": "enabled",
                },
            }),
        }],
    }
}

fn openai_capabilities() -> ModelCapabilities {
    ModelCapabilities {
        tools: true,
        parallel_tools: true,
        reasoning_replay: false,
        strict_schema: false,
        continuation: true,
    }
}

fn deepseek_capabilities(with_reasoning: bool) -> ModelCapabilities {
    ModelCapabilities {
        tools: true,
        parallel_tools: false,
        reasoning_replay: with_reasoning,
        strict_schema: false,
        continuation: false,
    }
}

fn mimo_capabilities(with_reasoning: bool) -> ModelCapabilities {
    ModelCapabilities {
        tools: true,
        parallel_tools: false,
        reasoning_replay: with_reasoning,
        strict_schema: false,
        continuation: false,
    }
}

fn openai_request_policy() -> ModelRequestPolicy {
    ModelRequestPolicy {
        max_tokens_field: "max_output_tokens".to_string(),
        store: Some(true),
        ..ModelRequestPolicy::default()
    }
}

fn chat_request_policy(max_tokens_field: &str) -> ModelRequestPolicy {
    ModelRequestPolicy {
        max_tokens_field: max_tokens_field.to_string(),
        ..ModelRequestPolicy::default()
    }
}

fn fallback_model(id: &str) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens: 128_000,
        output_tokens: 8_192,
        supports_tools: true,
        wire_api: ModelWireApi::Responses,
        capabilities: ModelCapabilities::default(),
        request_policy: ModelRequestPolicy::default(),
        reasoning: None,
        options: serde_json::Value::Null,
        headers: BTreeMap::new(),
    }
}

fn resolve_api_key(api_key: Option<String>, api_key_env: Option<&str>) -> String {
    api_key
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            api_key_env
                .filter(|name| !name.trim().is_empty())
                .and_then(|name| std::env::var(name).ok())
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_default()
}

fn normalized_id(value: &str) -> String {
    value.trim().to_string()
}

fn is_null(value: &serde_json::Value) -> bool {
    value.is_null()
}

fn validate_providers_toml(file: &ProvidersToml) -> Result<()> {
    let providers = file
        .providers
        .iter()
        .map(|(id, provider)| ProviderConfig {
            id: id.clone(),
            kind: provider.kind,
            name: provider.name.clone(),
            base_url: provider.base_url.clone(),
            api_key: provider.api_key.clone(),
            api_key_env: provider.api_key_env.clone(),
            default_model: provider.default_model.clone(),
            enabled: provider.enabled,
            models: provider
                .models
                .iter()
                .map(|(model_id, model)| model.clone().into_model(model_id.clone(), provider.kind))
                .collect(),
        })
        .collect();
    validate_provider_request(&ProvidersConfigRequest {
        default_provider_id: file.default_provider_id.clone(),
        providers,
    })
}

fn validate_provider_request(request: &ProvidersConfigRequest) -> Result<()> {
    let mut ids = BTreeSet::new();
    for provider in &request.providers {
        if provider.id.trim().is_empty() {
            return Err(StoreError::InvalidConfig(
                "provider id is required".to_string(),
            ));
        }
        if provider.name.trim().is_empty() {
            return Err(StoreError::InvalidConfig(format!(
                "provider `{}` name is required",
                provider.id
            )));
        }
        if provider.base_url.trim().is_empty() {
            return Err(StoreError::InvalidConfig(format!(
                "provider `{}` base_url is required",
                provider.id
            )));
        }
        let models = normalized_models(provider);
        let default_model = provider.default_model.trim();
        if default_model.is_empty() {
            return Err(StoreError::InvalidConfig(format!(
                "provider `{}` default_model is required",
                provider.id
            )));
        }
        if !models.iter().any(|model| model.id == default_model) {
            return Err(StoreError::InvalidConfig(format!(
                "provider `{}` default_model `{default_model}` is not in models",
                provider.id
            )));
        }
        for model in &models {
            if model.context_tokens == 0 || model.output_tokens == 0 {
                return Err(StoreError::InvalidConfig(format!(
                    "model `{}` must configure context_tokens and output_tokens",
                    model.id
                )));
            }
            if model.request_policy.max_tokens_field.trim().is_empty() {
                return Err(StoreError::InvalidConfig(format!(
                    "model `{}` request_policy.max_tokens_field is required",
                    model.id
                )));
            }
            if !(model.request_policy.extra_body.is_null()
                || model.request_policy.extra_body.is_object())
            {
                return Err(StoreError::InvalidConfig(format!(
                    "model `{}` request_policy.extra_body must be an object",
                    model.id
                )));
            }
            if let Some(reasoning) = &model.reasoning {
                if reasoning.variants.is_empty() {
                    return Err(StoreError::InvalidConfig(format!(
                        "reasoning model `{}` must configure reasoning variants",
                        model.id
                    )));
                }
                let mut variants = BTreeSet::new();
                for variant in &reasoning.variants {
                    let id = variant.id.trim();
                    if id.is_empty() {
                        return Err(StoreError::InvalidConfig(format!(
                            "model `{}` reasoning variant id is required",
                            model.id
                        )));
                    }
                    if !variant.request.is_object() {
                        return Err(StoreError::InvalidConfig(format!(
                            "model `{}` reasoning variant `{id}` request must be an object",
                            model.id
                        )));
                    }
                    if !variants.insert(id.to_string()) {
                        return Err(StoreError::InvalidConfig(format!(
                            "model `{}` has duplicate reasoning variant `{id}`",
                            model.id
                        )));
                    }
                }
                if let Some(default_variant) = reasoning.default_variant.as_deref()
                    && !variants.contains(default_variant.trim())
                {
                    return Err(StoreError::InvalidConfig(format!(
                        "model `{}` default reasoning variant is not in reasoning variants",
                        model.id
                    )));
                }
            }
        }
        if !ids.insert(provider.id.trim().to_string()) {
            return Err(StoreError::InvalidConfig(format!(
                "duplicate provider id `{}`",
                provider.id
            )));
        }
    }
    if let Some(default_provider_id) = request.default_provider_id.as_deref()
        && !default_provider_id.trim().is_empty()
        && !ids.contains(default_provider_id.trim())
    {
        return Err(StoreError::InvalidConfig(format!(
            "default provider `{default_provider_id}` is not in providers"
        )));
    }
    Ok(())
}

fn normalized_models(provider: &ProviderConfig) -> Vec<ModelConfig> {
    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    for model in provider.models.iter().cloned() {
        let id = model.id.trim().to_string();
        if !id.is_empty() && seen.insert(id.clone()) {
            models.push(ModelConfig { id, ..model });
        }
    }
    models
}

fn session_context_tokens_key(agent_id: AgentId, session_id: SessionId) -> String {
    format!("session_context_tokens:{agent_id}:{session_id}")
}

fn parse_agent_id(value: &str) -> Result<AgentId> {
    Uuid::parse_str(value)
        .map_err(|err| StoreError::InvalidConfig(format!("invalid agent id `{value}`: {err}")))
}

fn parse_task_id(value: &str) -> Result<TaskId> {
    Uuid::parse_str(value)
        .map_err(|err| StoreError::InvalidConfig(format!("invalid task id `{value}`: {err}")))
}

fn parse_project_id(value: &str) -> Result<ProjectId> {
    Uuid::parse_str(value)
        .map_err(|err| StoreError::InvalidConfig(format!("invalid project id `{value}`: {err}")))
}

fn parse_session_id(value: &str) -> Result<SessionId> {
    Uuid::parse_str(value)
        .map_err(|err| StoreError::InvalidConfig(format!("invalid session id `{value}`: {err}")))
}

fn parse_turn_id(value: &str) -> Result<TurnId> {
    Uuid::parse_str(value)
        .map_err(|err| StoreError::InvalidConfig(format!("invalid turn id `{value}`: {err}")))
}

fn parse_uuid(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value)
        .map_err(|err| StoreError::InvalidConfig(format!("invalid uuid `{value}`: {err}")))
}

fn parse_utc(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

fn parse_store_enum<T>(value: &str) -> Result<T>
where
    T: FromStr<Err = strum::ParseError>,
{
    if let Ok(parsed) = value.parse() {
        return Ok(parsed);
    }
    let mut normalized = String::with_capacity(value.len() + 4);
    for (index, ch) in value.char_indices() {
        if ch.is_ascii_uppercase() {
            if index > 0 && !normalized.ends_with('_') {
                normalized.push('_');
            }
            normalized.push(ch.to_ascii_lowercase());
        } else if ch == '-' {
            normalized.push('_');
        } else {
            normalized.push(ch);
        }
    }
    Ok(normalized.parse()?)
}

fn u64_to_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
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

fn event_session_id(event: &ServiceEvent) -> Option<SessionId> {
    match &event.kind {
        ServiceEventKind::TurnStarted { session_id, .. }
        | ServiceEventKind::TurnCompleted { session_id, .. }
        | ServiceEventKind::ToolStarted { session_id, .. }
        | ServiceEventKind::ToolCompleted { session_id, .. }
        | ServiceEventKind::AgentMessage { session_id, .. }
        | ServiceEventKind::SkillsActivated { session_id, .. }
        | ServiceEventKind::UserInputRequested { session_id, .. } => *session_id,
        ServiceEventKind::ContextCompacted { session_id, .. } => Some(*session_id),
        ServiceEventKind::Error { session_id, .. } => *session_id,
        _ => None,
    }
}

fn github_app_install_url(app_slug: Option<&str>) -> Option<String> {
    app_slug
        .map(str::trim)
        .filter(|slug| !slug.is_empty())
        .map(|slug| format!("https://github.com/apps/{slug}/installations/new"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{
        AgentStatus, McpServerScope, McpServerTransport, MessageRole, ModelContentItem,
        ModelToolCall, ProjectCloneStatus, ProjectReviewOutcome, ProjectReviewRunStatus,
        ProjectReviewStatus, ProjectStatus, ServiceEventKind, TurnStatus,
    };
    use serde_json::json;
    use tempfile::{TempDir, tempdir};

    async fn store() -> (TempDir, ConfigStore) {
        let dir = tempdir().expect("tempdir");
        let store = ConfigStore::open_with_config_and_artifact_index_path(
            dir.path().join("config.sqlite3"),
            dir.path().join("config.toml"),
            dir.path().join("artifacts/index"),
        )
        .await
        .expect("open store");
        (dir, store)
    }

    fn provider(api_key: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            id: "openai".to_string(),
            kind: ProviderKind::Openai,
            name: "OpenAI".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: api_key.map(str::to_string),
            api_key_env: Some("OPENAI_API_KEY".to_string()),
            models: vec![test_model("gpt-5.5"), test_model("gpt-5.4")],
            default_model: "gpt-5.5".to_string(),
            enabled: true,
        }
    }

    fn test_model(id: &str) -> ModelConfig {
        ModelConfig {
            id: id.to_string(),
            name: Some(id.to_string()),
            context_tokens: 400_000,
            output_tokens: 128_000,
            supports_tools: true,
            reasoning: Some(ModelReasoningConfig {
                default_variant: Some("medium".to_string()),
                variants: ["minimal", "low", "medium", "high"]
                    .into_iter()
                    .map(|id| ModelReasoningVariant {
                        id: id.to_string(),
                        label: None,
                        request: json!({
                            "reasoning": {
                                "effort": id,
                            },
                        }),
                    })
                    .collect(),
            }),
            options: serde_json::Value::Null,
            headers: BTreeMap::new(),
            wire_api: ModelWireApi::Responses,
            capabilities: ModelCapabilities::default(),
            request_policy: ModelRequestPolicy::default(),
        }
    }

    #[tokio::test]
    async fn provider_response_is_redacted_and_preserves_empty_key() {
        let (_dir, store) = store().await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![provider(Some("secret"))],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save");
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![provider(Some(""))],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save preserve");

        let response = store.providers_response().await.expect("providers");
        assert!(response.providers[0].has_api_key);
        let resolved = store
            .resolve_provider(Some("openai"), Some("gpt-5.4"))
            .await
            .expect("resolve");
        assert_eq!(resolved.provider.api_key, "secret");
        assert_eq!(resolved.model.id, "gpt-5.4");
    }

    #[tokio::test]
    async fn provider_cache_reloads_when_config_file_changes() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let store =
            ConfigStore::open_with_config_path(dir.path().join("config.sqlite3"), &config_path)
                .await
                .expect("open");
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![provider(Some("first-secret"))],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save");
        assert_eq!(
            store
                .resolve_provider(Some("openai"), Some("gpt-5.4"))
                .await
                .expect("resolve")
                .provider
                .api_key,
            "first-secret"
        );

        let text = std::fs::read_to_string(&config_path)
            .expect("read config")
            .replace("first-secret", "second-secret-longer");
        std::fs::write(&config_path, text).expect("write config");
        assert_eq!(
            store
                .resolve_provider(Some("openai"), Some("gpt-5.4"))
                .await
                .expect("resolve changed")
                .provider
                .api_key,
            "second-secret-longer"
        );
    }

    #[tokio::test]
    async fn artifacts_use_configured_index_dir() {
        let dir = tempdir().expect("tempdir");
        let index_dir = dir.path().join("artifact-index");
        let store = ConfigStore::open_with_config_and_artifact_index_path(
            dir.path().join("config.sqlite3"),
            dir.path().join("config.toml"),
            &index_dir,
        )
        .await
        .expect("open store");
        let task_id = Uuid::new_v4();
        let artifact = ArtifactInfo {
            id: "artifact-1".to_string(),
            agent_id: Uuid::new_v4(),
            task_id,
            name: "report.txt".to_string(),
            path: "/workspace/report.txt".to_string(),
            size_bytes: 7,
            created_at: Utc::now(),
        };

        store.save_artifact(&artifact).expect("save artifact");

        assert!(index_dir.join("artifact-1.json").exists());
        assert!(!dir.path().join("artifacts/index/artifact-1.json").exists());
        let artifacts = store.load_artifacts(&task_id).expect("load artifacts");
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].id, artifact.id);
        assert_eq!(artifacts[0].task_id, artifact.task_id);
        assert_eq!(artifacts[0].name, artifact.name);

        let all_artifacts = store.load_all_artifacts().expect("load all artifacts");
        assert_eq!(all_artifacts.len(), 1);
        assert_eq!(all_artifacts[0].id, artifact.id);
    }

    #[tokio::test]
    async fn git_account_save_enters_verifying_and_clears_previous_error() {
        let (_dir, store) = store().await;
        let saved = store
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
        assert_eq!(saved.status, GitAccountStatus::Verifying);
        assert_eq!(saved.last_error, None);
        assert_eq!(saved.last_verified_at, None);

        let failed = store
            .update_git_account_verification(
                "account-1",
                None,
                GitTokenKind::Unknown,
                Vec::new(),
                GitAccountStatus::Failed,
                Some("bad token".to_string()),
            )
            .await
            .expect("mark failed");
        assert_eq!(failed.status, GitAccountStatus::Failed);
        assert!(failed.last_verified_at.is_some());

        let resaved = store
            .upsert_git_account(GitAccountRequest {
                id: Some("account-1".to_string()),
                provider: GitProvider::Github,
                label: "Personal".to_string(),
                token: Some("new-secret".to_string()),
                is_default: true,
                ..Default::default()
            })
            .await
            .expect("resave account");
        assert_eq!(resaved.status, GitAccountStatus::Verifying);
        assert_eq!(resaved.last_error, None);
        assert_eq!(resaved.last_verified_at, None);
    }

    #[tokio::test]
    async fn git_account_delete_wins_over_late_verification_update() {
        let (_dir, store) = store().await;
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

        let response = store
            .delete_git_account("account-1")
            .await
            .expect("delete account");
        assert!(response.accounts.is_empty());
        assert_eq!(response.default_account_id, None);

        let late_update = store
            .update_git_account_verification(
                "account-1",
                Some("octo".to_string()),
                GitTokenKind::Classic,
                vec!["repo".to_string()],
                GitAccountStatus::Verified,
                None,
            )
            .await;
        assert!(late_update.is_err());

        let response = store.list_git_accounts().await.expect("list accounts");
        assert!(response.accounts.is_empty());
        assert_eq!(response.default_account_id, None);
    }

    #[tokio::test]
    async fn rejects_unknown_model() {
        let (_dir, store) = store().await;
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![provider(Some("secret"))],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save");
        assert!(
            store
                .resolve_provider(Some("openai"), Some("unknown"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn agent_config_defaults_when_missing_and_clears_invalid_json() {
        let (_dir, store) = store().await;
        assert_eq!(
            store.load_agent_config().await.expect("missing config"),
            AgentConfigRequest::default()
        );
        store
            .set_setting(SETTING_AGENT_CONFIG, "{not json")
            .await
            .expect("write invalid");
        assert_eq!(
            store.load_agent_config().await.expect("invalid config"),
            AgentConfigRequest::default()
        );
        assert_eq!(
            store
                .get_setting(SETTING_AGENT_CONFIG)
                .await
                .expect("setting"),
            None
        );
        store
            .set_setting(
                SETTING_AGENT_CONFIG,
                r#"{"research_agent":{"provider_id":"openai","model":"gpt-5.4"}}"#,
            )
            .await
            .expect("write old config");
        assert_eq!(
            store.load_agent_config().await.expect("old config"),
            AgentConfigRequest::default()
        );
        assert_eq!(
            store
                .get_setting(SETTING_AGENT_CONFIG)
                .await
                .expect("setting"),
            None
        );
    }

    #[tokio::test]
    async fn agent_config_persists_and_reloads() {
        let (dir, store) = store().await;
        let config = AgentConfigRequest {
            planner: None,
            explorer: None,
            executor: Some(mai_protocol::AgentModelPreference {
                provider_id: "openai".to_string(),
                model: "gpt-5.4".to_string(),
                reasoning_effort: Some("high".to_string()),
            }),
            reviewer: None,
        };
        store.save_agent_config(&config).await.expect("save config");
        drop(store);

        let reopened = ConfigStore::open_with_config_path(
            dir.path().join("config.sqlite3"),
            dir.path().join("config.toml"),
        )
        .await
        .expect("reopen");
        assert_eq!(
            reopened.load_agent_config().await.expect("load config"),
            config
        );
    }

    #[tokio::test]
    async fn provider_presets_include_builtin_metadata() {
        let (_dir, store) = store().await;
        let presets = store.provider_presets_response();
        let openai = presets
            .providers
            .iter()
            .find(|provider| provider.kind == ProviderKind::Openai)
            .expect("openai preset");
        let deepseek = presets
            .providers
            .iter()
            .find(|provider| provider.kind == ProviderKind::Deepseek)
            .expect("deepseek preset");
        assert_eq!(openai.default_model, "gpt-5.5");
        assert!(openai.models.iter().any(|model| model.id == "gpt-5.4-mini"));
        assert_eq!(deepseek.default_model, "deepseek-v4-flash");
        let v4_pro = deepseek
            .models
            .iter()
            .find(|model| model.id == "deepseek-v4-pro")
            .expect("deepseek v4 pro");
        assert_eq!(v4_pro.context_tokens, DEEPSEEK_V4_CONTEXT_TOKENS);
        assert_eq!(v4_pro.output_tokens, DEEPSEEK_V4_OUTPUT_TOKENS);
        let reasoning = v4_pro.reasoning.as_ref().expect("reasoning variants");
        assert_eq!(reasoning.default_variant.as_deref(), Some("high"));
        assert_eq!(
            reasoning
                .variants
                .iter()
                .map(|variant| variant.id.as_str())
                .collect::<Vec<_>>(),
            vec!["high", "max"]
        );
        for id in [
            "deepseek-v4-flash",
            "deepseek-v4-pro",
            "deepseek-chat",
            "deepseek-reasoner",
        ] {
            let model = deepseek
                .models
                .iter()
                .find(|model| model.id == id)
                .expect("deepseek v4 model");
            assert_eq!(model.context_tokens, DEEPSEEK_V4_CONTEXT_TOKENS);
            assert_eq!(model.output_tokens, DEEPSEEK_V4_OUTPUT_TOKENS);
        }
        assert!(
            deepseek
                .models
                .iter()
                .any(|model| model.id == "deepseek-reasoner")
        );
        let mimo_presets: Vec<_> = presets
            .providers
            .iter()
            .filter(|provider| provider.kind == ProviderKind::Mimo)
            .collect();
        assert_eq!(
            mimo_presets.len(),
            2,
            "expected mimo-api and mimo-token-plan presets"
        );
        let mimo_api = mimo_presets
            .iter()
            .find(|p| p.id == "mimo-api")
            .expect("mimo-api preset");
        let mimo_tp = mimo_presets
            .iter()
            .find(|p| p.id == "mimo-token-plan")
            .expect("mimo-token-plan preset");
        assert_eq!(mimo_api.base_url, "https://api.xiaomimimo.com/v1");
        assert_eq!(mimo_tp.base_url, "https://token-plan-cn.xiaomimimo.com/v1");
        assert_eq!(mimo_api.default_model, "mimo-v2.5-pro");

        for (id, context_tokens, output_tokens) in [
            ("mimo-v2.5-pro", 1_000_000, 131_072),
            ("mimo-v2.5", 1_000_000, 131_072),
            ("mimo-v2-pro", 1_000_000, 131_072),
            ("mimo-v2-omni", 256_000, 131_072),
            ("mimo-v2-flash", 256_000, 65_536),
        ] {
            let model = mimo_api
                .models
                .iter()
                .find(|model| model.id == id)
                .expect("mimo model");
            assert_eq!(model.context_tokens, context_tokens);
            assert_eq!(model.output_tokens, output_tokens);
        }

        let mimo_pro = mimo_api
            .models
            .iter()
            .find(|model| model.id == "mimo-v2.5-pro")
            .expect("mimo-v2.5-pro");
        assert!(mimo_pro.reasoning.is_some());
        let mimo_flash = mimo_api
            .models
            .iter()
            .find(|model| model.id == "mimo-v2-flash")
            .expect("mimo-v2-flash");
        assert!(mimo_flash.reasoning.is_none());
    }

    #[tokio::test]
    async fn provider_toml_preserves_custom_model_metadata() {
        let (_dir, store) = store().await;
        let mut provider = provider(Some("secret"));
        let mut custom = test_model("custom-chat");
        custom.context_tokens = 123_456;
        custom.output_tokens = 4_096;
        custom.supports_tools = false;
        custom.reasoning = None;
        custom.options = json!({ "temperature": 0.2 });
        custom
            .headers
            .insert("X-Test-Model".to_string(), "custom".to_string());
        provider.models.push(custom);
        provider.default_model = "custom-chat".to_string();
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![provider],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save");

        let response = store.providers_response().await.expect("providers");
        let model = response.providers[0]
            .models
            .iter()
            .find(|model| model.id == "custom-chat")
            .expect("custom model");
        assert_eq!(model.context_tokens, 123_456);
        assert!(!model.supports_tools);
        assert_eq!(model.options["temperature"], json!(0.2));
        assert_eq!(model.headers["X-Test-Model"], "custom");
    }

    #[tokio::test]
    async fn legacy_deepseek_models_migrate_to_chat_policy() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
                default_provider_id = "deepseek"

                [providers.deepseek]
                kind = "deepseek"
                name = "DeepSeek"
                base_url = "https://api.deepseek.com"
                api_key = "secret"
                default_model = "deepseek-v4-pro"
                enabled = true

                [providers.deepseek.models.deepseek-v4-pro]
                name = "deepseek-v4-pro"
                context_tokens = 1000000
                output_tokens = 384000
                supports_tools = true
            "#,
        )
        .expect("write config");
        let store =
            ConfigStore::open_with_config_path(dir.path().join("config.sqlite3"), &config_path)
                .await
                .expect("open");

        let response = store.providers_response().await.expect("providers");
        let model = response.providers[0].models.first().expect("model");
        assert_eq!(model.wire_api, ModelWireApi::ChatCompletions);
        assert!(!model.capabilities.continuation);
        assert!(model.capabilities.reasoning_replay);
        assert_eq!(model.request_policy.store, None);
        assert_eq!(model.request_policy.max_tokens_field, "max_tokens");
    }

    #[tokio::test]
    async fn old_provider_toml_schema_is_rebuilt() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
                [mcp_servers.demo]
                command = "demo-mcp"
            "#,
        )
        .expect("write old config");
        let store =
            ConfigStore::open_with_config_path(dir.path().join("config.sqlite3"), &config_path)
                .await
                .expect("open");

        assert_eq!(store.provider_count().await.expect("count"), 0);
        assert!(!config_path.exists());
    }

    #[tokio::test]
    async fn old_reasoning_toml_schema_is_rebuilt() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
                default_provider_id = "deepseek"

                [providers.deepseek]
                kind = "deepseek"
                name = "DeepSeek"
                base_url = "https://api.deepseek.com"
                api_key = "secret"
                default_model = "deepseek-v4-pro"
                enabled = true

                [providers.deepseek.models.deepseek-v4-pro]
                name = "deepseek-v4-pro"
                context_tokens = 128000
                output_tokens = 8192
                supports_tools = true
                supports_reasoning = true
                reasoning_efforts = ["high", "max"]
                default_reasoning_effort = "high"
            "#,
        )
        .expect("write old config");
        let store =
            ConfigStore::open_with_config_path(dir.path().join("config.sqlite3"), &config_path)
                .await
                .expect("open");

        assert_eq!(store.provider_count().await.expect("count"), 0);
        assert!(!config_path.exists());
    }

    #[tokio::test]
    async fn runtime_snapshot_survives_reopen() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("config.sqlite3");
        let store = ConfigStore::open_with_config_path(&db_path, dir.path().join("config.toml"))
            .await
            .expect("open");
        let agent_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        let now = Utc::now();
        let summary = AgentSummary {
            id: agent_id,
            parent_id: None,
            task_id: None,
            project_id: None,
            role: None,
            name: "agent-test".to_string(),
            status: AgentStatus::Completed,
            container_id: Some("container".to_string()),
            docker_image: "ghcr.io/rcore-os/tgoskits-container:latest".to_string(),
            provider_id: "openai".to_string(),
            provider_name: "OpenAI".to_string(),
            model: "gpt-5.2".to_string(),
            reasoning_effort: Some("high".to_string()),
            created_at: now,
            updated_at: now,
            current_turn: Some(turn_id),
            last_error: None,
            token_usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
                total_tokens: 3,
            },
        };
        let session = AgentSessionSummary {
            id: session_id,
            title: "Chat 1".to_string(),
            created_at: now,
            updated_at: now,
            message_count: 0,
        };
        let message = AgentMessage {
            role: MessageRole::User,
            content: "hello".to_string(),
            created_at: now,
        };
        let history = [
            ModelInputItem::Message {
                role: "user".to_string(),
                content: vec![ModelContentItem::InputText {
                    text: "hello".to_string(),
                }],
            },
            ModelInputItem::AssistantTurn {
                content: None,
                reasoning_content: Some("thinking".to_string()),
                tool_calls: vec![ModelToolCall {
                    call_id: "call_1".to_string(),
                    name: "container_exec".to_string(),
                    arguments: "{\"command\":\"pwd\"}".to_string(),
                }],
            },
        ];
        let event = ServiceEvent {
            sequence: 7,
            timestamp: now,
            kind: ServiceEventKind::AgentMessage {
                agent_id,
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                role: MessageRole::User,
                content: "hello".to_string(),
            },
        };

        store
            .save_agent(&summary, Some("system"))
            .await
            .expect("save agent");
        store
            .save_agent_session(agent_id, &session)
            .await
            .expect("save session");
        store
            .append_agent_message(agent_id, session_id, 0, &message)
            .await
            .expect("message");
        store
            .append_agent_history_item(agent_id, session_id, 0, &history[0])
            .await
            .expect("history");
        store
            .append_agent_history_item(agent_id, session_id, 1, &history[1])
            .await
            .expect("history");
        store.append_service_event(&event).await.expect("event");
        drop(store);

        let reopened = ConfigStore::open_with_config_path(&db_path, dir.path().join("config.toml"))
            .await
            .expect("reopen");
        let snapshot = reopened.load_runtime_snapshot(500).await.expect("snapshot");
        assert_eq!(snapshot.next_sequence, 8);
        assert_eq!(snapshot.agents.len(), 1);
        assert_eq!(snapshot.agents[0].summary.name, "agent-test");
        assert_eq!(
            snapshot.agents[0].summary.docker_image,
            "ghcr.io/rcore-os/tgoskits-container:latest"
        );
        assert_eq!(snapshot.agents[0].system_prompt.as_deref(), Some("system"));
        assert_eq!(snapshot.agents[0].sessions.len(), 1);
        assert_eq!(snapshot.agents[0].sessions[0].summary.title, "Chat 1");
        assert_eq!(snapshot.agents[0].sessions[0].summary.message_count, 1);
        assert_eq!(snapshot.agents[0].sessions[0].history.len(), 2);
        assert_eq!(snapshot.agents[0].sessions[0].last_context_tokens, None);
        assert!(matches!(
            &snapshot.agents[0].sessions[0].history[1],
            ModelInputItem::AssistantTurn {
                reasoning_content: Some(reasoning),
                tool_calls,
                ..
            } if reasoning == "thinking"
                && tool_calls.len() == 1
                && tool_calls[0].call_id == "call_1"
        ));
        assert_eq!(snapshot.recent_events.len(), 1);
        assert_eq!(
            event_session_id(&snapshot.recent_events[0]),
            Some(session_id)
        );

        reopened.delete_agent(agent_id).await.expect("delete agent");
        let snapshot = reopened.load_runtime_snapshot(500).await.expect("snapshot");
        assert!(snapshot.agents.is_empty());
        assert!(
            reopened
                .load_agent_sessions(agent_id)
                .await
                .expect("sessions")
                .is_empty()
        );
        assert!(
            reopened
                .load_agent_messages(agent_id, session_id)
                .await
                .expect("messages")
                .is_empty()
        );
        assert!(
            reopened
                .load_agent_history(agent_id, session_id)
                .await
                .expect("history")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn replace_agent_history_only_replaces_target_session() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("config.sqlite3");
        let store = ConfigStore::open_with_config_path(&db_path, dir.path().join("config.toml"))
            .await
            .expect("open");
        let agent_id = Uuid::new_v4();
        let first_session_id = Uuid::new_v4();
        let second_session_id = Uuid::new_v4();
        let now = Utc::now();
        let summary = AgentSummary {
            id: agent_id,
            parent_id: None,
            task_id: None,
            project_id: None,
            role: None,
            name: "agent-test".to_string(),
            status: AgentStatus::Completed,
            container_id: None,
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "openai".to_string(),
            provider_name: "OpenAI".to_string(),
            model: "gpt-5.2".to_string(),
            reasoning_effort: None,
            created_at: now,
            updated_at: now,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        };
        store.save_agent(&summary, None).await.expect("save agent");
        for session_id in [first_session_id, second_session_id] {
            store
                .save_agent_session(
                    agent_id,
                    &AgentSessionSummary {
                        id: session_id,
                        title: "Chat".to_string(),
                        created_at: now,
                        updated_at: now,
                        message_count: 0,
                    },
                )
                .await
                .expect("save session");
        }
        store
            .append_agent_history_item(
                agent_id,
                first_session_id,
                0,
                &ModelInputItem::user_text("old first"),
            )
            .await
            .expect("first history");
        store
            .append_agent_history_item(
                agent_id,
                second_session_id,
                0,
                &ModelInputItem::user_text("old second"),
            )
            .await
            .expect("second history");

        store
            .replace_agent_history(
                agent_id,
                first_session_id,
                &[ModelInputItem::user_text("new")],
            )
            .await
            .expect("replace");
        let first = store
            .load_agent_history(agent_id, first_session_id)
            .await
            .expect("first");
        let second = store
            .load_agent_history(agent_id, second_session_id)
            .await
            .expect("second");
        assert!(matches!(
            &first[0],
            ModelInputItem::Message { content, .. }
                if matches!(&content[0], ModelContentItem::InputText { text } if text == "new")
        ));
        assert!(matches!(
            &second[0],
            ModelInputItem::Message { content, .. }
                if matches!(&content[0], ModelContentItem::InputText { text } if text == "old second")
        ));
    }

    #[tokio::test]
    async fn session_context_tokens_survive_reopen_and_clear() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("config.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open");
        let agent_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let now = Utc::now();
        store
            .save_agent(
                &AgentSummary {
                    id: agent_id,
                    parent_id: None,
                    task_id: None,
                    project_id: None,
                    role: None,
                    name: "agent-test".to_string(),
                    status: AgentStatus::Completed,
                    container_id: None,
                    docker_image: "ubuntu:latest".to_string(),
                    provider_id: "openai".to_string(),
                    provider_name: "OpenAI".to_string(),
                    model: "gpt-5.2".to_string(),
                    reasoning_effort: None,
                    created_at: now,
                    updated_at: now,
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
                    id: session_id,
                    title: "Chat".to_string(),
                    created_at: now,
                    updated_at: now,
                    message_count: 0,
                },
            )
            .await
            .expect("save session");
        store
            .save_session_context_tokens(agent_id, session_id, 1234)
            .await
            .expect("save tokens");
        drop(store);

        let reopened = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("reopen");
        let snapshot = reopened.load_runtime_snapshot(10).await.expect("snapshot");
        assert_eq!(
            snapshot.agents[0].sessions[0].last_context_tokens,
            Some(1234)
        );
        reopened
            .clear_session_context_tokens(agent_id, session_id)
            .await
            .expect("clear");
        let snapshot = reopened.load_runtime_snapshot(10).await.expect("snapshot");
        assert_eq!(snapshot.agents[0].sessions[0].last_context_tokens, None);
    }

    #[tokio::test]
    async fn project_review_runs_round_trip_and_prune() {
        let (_dir, store) = store().await;
        let project_id = Uuid::new_v4();
        let reviewer_agent_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let started_at = Utc::now() - chrono::TimeDelta::days(1);
        let finished_at = started_at + chrono::TimeDelta::minutes(3);
        store
            .save_project_review_run(&ProjectReviewRunDetail {
                summary: ProjectReviewRunSummary {
                    id: run_id,
                    project_id,
                    reviewer_agent_id: Some(reviewer_agent_id),
                    turn_id: Some(turn_id),
                    started_at,
                    finished_at: Some(finished_at),
                    status: ProjectReviewRunStatus::Completed,
                    outcome: Some(ProjectReviewOutcome::ReviewSubmitted),
                    pr: Some(42),
                    summary: Some("approved".to_string()),
                    error: None,
                },
                messages: vec![AgentMessage {
                    role: MessageRole::Assistant,
                    content: "done".to_string(),
                    created_at: finished_at,
                }],
                events: vec![ServiceEvent {
                    sequence: 1,
                    timestamp: finished_at,
                    kind: ServiceEventKind::TurnCompleted {
                        agent_id: reviewer_agent_id,
                        session_id: None,
                        turn_id,
                        status: TurnStatus::Completed,
                    },
                }],
            })
            .await
            .expect("save run");

        let runs = store
            .load_project_review_runs(project_id, None, 0, 10)
            .await
            .expect("runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].pr, Some(42));
        assert_eq!(runs[0].outcome, Some(ProjectReviewOutcome::ReviewSubmitted));
        let detail = store
            .load_project_review_run(project_id, run_id)
            .await
            .expect("detail")
            .expect("run exists");
        assert_eq!(detail.messages[0].content, "done");
        assert_eq!(detail.events.len(), 1);

        let removed = store
            .prune_project_review_runs_before(Utc::now() - chrono::TimeDelta::days(2))
            .await
            .expect("no prune");
        assert_eq!(removed, 0);
        let removed = store
            .prune_project_review_runs_before(Utc::now())
            .await
            .expect("prune");
        assert_eq!(removed, 1);
        assert!(
            store
                .load_project_review_run(project_id, run_id)
                .await
                .expect("load")
                .is_none()
        );
    }

    #[tokio::test]
    async fn agent_logs_round_trip_filter_and_prune() {
        let (_dir, store) = store().await;
        let agent_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        let old_time = Utc::now() - chrono::TimeDelta::days(6);
        let new_time = Utc::now();

        store
            .append_agent_log_entry(&AgentLogEntry {
                id: Uuid::new_v4(),
                agent_id,
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                level: "info".to_string(),
                category: "tool".to_string(),
                message: "tool started".to_string(),
                details: json!({ "call_id": "call_1" }),
                timestamp: new_time,
            })
            .await
            .expect("save new log");
        store
            .append_agent_log_entry(&AgentLogEntry {
                id: Uuid::new_v4(),
                agent_id,
                session_id: None,
                turn_id: None,
                level: "warn".to_string(),
                category: "model".to_string(),
                message: "old".to_string(),
                details: json!({}),
                timestamp: old_time,
            })
            .await
            .expect("save old log");

        let logs = store
            .list_agent_logs(
                agent_id,
                AgentLogFilter {
                    session_id: Some(session_id),
                    turn_id: Some(turn_id),
                    level: Some("info".to_string()),
                    category: Some("tool".to_string()),
                    limit: 100,
                    ..Default::default()
                },
            )
            .await
            .expect("list logs");
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].message, "tool started");
        assert_eq!(logs[0].details["call_id"], "call_1");

        let removed = store
            .prune_agent_logs_before(Utc::now() - chrono::TimeDelta::days(5))
            .await
            .expect("prune logs");
        assert_eq!(removed, 1);
        let remaining = store
            .list_agent_logs(
                agent_id,
                AgentLogFilter {
                    limit: 100,
                    ..Default::default()
                },
            )
            .await
            .expect("remaining logs");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].category, "tool");
    }

    #[tokio::test]
    async fn tool_traces_round_trip_filter_and_prune() {
        let (_dir, store) = store().await;
        let agent_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        let old_time = Utc::now() - chrono::TimeDelta::days(6);
        let new_time = Utc::now();

        let trace = ToolTraceDetail {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            call_id: "call_1".to_string(),
            tool_name: "container_exec".to_string(),
            arguments: json!({ "command": "printf hi" }),
            output: r#"{"status":0,"stdout":"hi","stderr":""}"#.to_string(),
            success: true,
            duration_ms: Some(42),
            started_at: Some(new_time),
            completed_at: Some(new_time),
            output_preview: "hi".to_string(),
        };
        store
            .save_tool_trace_started(&trace, new_time)
            .await
            .expect("save start");
        store
            .save_tool_trace_completed(&trace, new_time, new_time)
            .await
            .expect("save completed");
        store
            .save_tool_trace_completed(
                &ToolTraceDetail {
                    call_id: "call_old".to_string(),
                    started_at: Some(old_time),
                    completed_at: Some(old_time),
                    output_preview: "old".to_string(),
                    ..trace.clone()
                },
                old_time,
                old_time,
            )
            .await
            .expect("save old");

        let summaries = store
            .list_tool_traces(
                agent_id,
                ToolTraceFilter {
                    session_id: Some(session_id),
                    turn_id: Some(turn_id),
                    limit: 100,
                    ..Default::default()
                },
            )
            .await
            .expect("list traces");
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].call_id, "call_1");
        assert_eq!(summaries[0].duration_ms, Some(42));

        let loaded = store
            .load_tool_trace(agent_id, Some(session_id), "call_1")
            .await
            .expect("load trace")
            .expect("trace");
        assert_eq!(loaded.arguments["command"], "printf hi");
        assert_eq!(loaded.output_preview, "hi");

        let removed = store
            .prune_tool_traces_before(Utc::now() - chrono::TimeDelta::days(5))
            .await
            .expect("prune traces");
        assert_eq!(removed, 1);
        let remaining = store
            .list_tool_traces(
                agent_id,
                ToolTraceFilter {
                    limit: 100,
                    ..Default::default()
                },
            )
            .await
            .expect("remaining traces");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].call_id, "call_1");
    }

    #[tokio::test]
    async fn tool_traces_keep_same_call_id_for_different_agents() {
        let (_dir, store) = store().await;
        let first_agent_id = Uuid::new_v4();
        let second_agent_id = Uuid::new_v4();
        let first_session_id = Uuid::new_v4();
        let second_session_id = Uuid::new_v4();
        let timestamp = Utc::now();

        for (agent_id, session_id, command) in [
            (first_agent_id, first_session_id, "pwd"),
            (second_agent_id, second_session_id, "ls"),
        ] {
            store
                .save_tool_trace_completed(
                    &ToolTraceDetail {
                        agent_id,
                        session_id: Some(session_id),
                        turn_id: Some(Uuid::new_v4()),
                        call_id: "call_duplicate".to_string(),
                        tool_name: "container_exec".to_string(),
                        arguments: json!({ "command": command }),
                        output: format!("{{\"command\":\"{command}\"}}"),
                        success: true,
                        duration_ms: Some(1),
                        started_at: Some(timestamp),
                        completed_at: Some(timestamp),
                        output_preview: command.to_string(),
                    },
                    timestamp,
                    timestamp,
                )
                .await
                .expect("save trace");
        }

        let first = store
            .load_tool_trace(first_agent_id, Some(first_session_id), "call_duplicate")
            .await
            .expect("load first")
            .expect("first trace");
        let second = store
            .load_tool_trace(second_agent_id, Some(second_session_id), "call_duplicate")
            .await
            .expect("load second")
            .expect("second trace");

        assert_eq!(first.arguments["command"], "pwd");
        assert_eq!(second.arguments["command"], "ls");
    }

    #[tokio::test]
    async fn delete_project_removes_review_runs() {
        let (_dir, store) = store().await;
        let project_id = Uuid::new_v4();
        let maintainer_agent_id = Uuid::new_v4();
        let timestamp = Utc::now();
        store
            .save_project(&ProjectSummary {
                id: project_id,
                name: "owner/repo".to_string(),
                status: ProjectStatus::Ready,
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                repository_full_name: "owner/repo".to_string(),
                git_account_id: Some("account-1".to_string()),
                repository_id: 42,
                installation_id: 0,
                installation_account: "owner".to_string(),
                branch: "main".to_string(),
                docker_image: "ubuntu:latest".to_string(),
                clone_status: ProjectCloneStatus::Ready,
                maintainer_agent_id,
                created_at: timestamp,
                updated_at: timestamp,
                last_error: None,
                auto_review_enabled: true,
                reviewer_extra_prompt: None,
                review_status: ProjectReviewStatus::Waiting,
                current_reviewer_agent_id: None,
                last_review_started_at: None,
                last_review_finished_at: None,
                next_review_at: None,
                last_review_outcome: None,
                review_last_error: None,
            })
            .await
            .expect("save project");
        store
            .save_project_review_run(&ProjectReviewRunDetail {
                summary: ProjectReviewRunSummary {
                    id: Uuid::new_v4(),
                    project_id,
                    reviewer_agent_id: None,
                    turn_id: None,
                    started_at: timestamp,
                    finished_at: None,
                    status: ProjectReviewRunStatus::Syncing,
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
        store
            .delete_project(project_id)
            .await
            .expect("delete project");
        assert!(
            store
                .load_project_review_runs(project_id, None, 0, 10)
                .await
                .expect("runs")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn invalid_sqlite_file_is_rebuilt() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.sqlite3");
        std::fs::write(&path, b"not sqlite").expect("write invalid old db");
        let store = ConfigStore::open_with_config_path(&path, dir.path().join("config.toml"))
            .await
            .expect("rebuild");
        assert_eq!(store.provider_count().await.expect("count"), 0);
    }

    #[tokio::test]
    async fn skills_config_persists_in_settings() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("config.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open");
        let config = SkillsConfigRequest {
            config: vec![mai_protocol::SkillConfigEntry {
                name: Some("demo".to_string()),
                path: None,
                enabled: false,
            }],
        };
        store
            .save_skills_config(&config)
            .await
            .expect("save skills config");
        drop(store);

        let reopened = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("reopen");
        assert_eq!(
            reopened
                .load_skills_config()
                .await
                .expect("load skills config"),
            config
        );
        assert!(
            !config_path.exists(),
            "provider config file should be untouched"
        );
    }

    #[tokio::test]
    async fn schema_version_mismatch_rebuilds_database() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("config.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open");
        store
            .save_mcp_servers(&BTreeMap::from([(
                "demo".to_string(),
                McpServerConfig {
                    command: Some("demo-mcp".to_string()),
                    ..Default::default()
                },
            )]))
            .await
            .expect("save server");
        store
            .set_setting(SETTING_SCHEMA_VERSION, "4")
            .await
            .expect("mark old schema");
        drop(store);

        let reopened = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("reopen");
        assert_eq!(
            reopened
                .get_setting(SETTING_SCHEMA_VERSION)
                .await
                .expect("schema marker")
                .as_deref(),
            Some(SCHEMA_VERSION)
        );
        assert!(
            reopened
                .list_mcp_servers()
                .await
                .expect("servers")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn schema_v8_migrates_projects_without_rebuild() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("config.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open");
        store
            .save_mcp_servers(&BTreeMap::from([(
                "demo".to_string(),
                McpServerConfig {
                    command: Some("demo-mcp".to_string()),
                    ..Default::default()
                },
            )]))
            .await
            .expect("save server");
        store
            .set_setting(SETTING_SCHEMA_VERSION, "8")
            .await
            .expect("mark v8 schema");
        drop(store);

        let reopened = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("reopen");
        assert_eq!(
            reopened
                .get_setting(SETTING_SCHEMA_VERSION)
                .await
                .expect("schema marker")
                .as_deref(),
            Some(SCHEMA_VERSION)
        );
        assert!(
            reopened
                .list_mcp_servers()
                .await
                .expect("servers")
                .contains_key("demo")
        );
        assert!(reopened.load_projects().await.expect("projects").is_empty());
    }

    #[tokio::test]
    async fn schema_v9_adds_project_create_fields_without_rebuild() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("config.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open");
        store
            .set_setting(SETTING_SCHEMA_VERSION, "9")
            .await
            .expect("mark v9 schema");
        drop(store);

        let reopened = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("reopen");
        assert_eq!(
            reopened
                .get_setting(SETTING_SCHEMA_VERSION)
                .await
                .expect("schema marker")
                .as_deref(),
            Some(SCHEMA_VERSION)
        );

        let conn = SqliteConnection::open(&db_path).expect("sqlite");
        assert!(sqlite_column_exists(&conn, "projects", "repository_full_name").expect("column"));
        assert!(sqlite_column_exists(&conn, "projects", "branch").expect("column"));
        assert!(!sqlite_column_exists(&conn, "projects", "project_path").expect("column"));
        assert!(!sqlite_column_exists(&conn, "projects", "workspace_path").expect("column"));
    }

    #[tokio::test]
    async fn schema_v11_drops_project_path_columns_without_rebuild() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("config.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open");
        let project_id = Uuid::new_v4();
        let maintainer_agent_id = Uuid::new_v4();
        let timestamp = Utc::now();
        store
            .save_project(&ProjectSummary {
                id: project_id,
                name: "owner/repo".to_string(),
                status: ProjectStatus::Ready,
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                repository_full_name: "owner/repo".to_string(),
                git_account_id: Some("account-1".to_string()),
                repository_id: 42,
                installation_id: 0,
                installation_account: "owner".to_string(),
                branch: "main".to_string(),
                docker_image: "ubuntu:latest".to_string(),
                clone_status: ProjectCloneStatus::Ready,
                maintainer_agent_id,
                created_at: timestamp,
                updated_at: timestamp,
                last_error: None,
                auto_review_enabled: true,
                reviewer_extra_prompt: Some("Focus on safety.".to_string()),
                review_status: ProjectReviewStatus::Waiting,
                current_reviewer_agent_id: Some(Uuid::new_v4()),
                last_review_started_at: Some(timestamp),
                last_review_finished_at: Some(timestamp),
                next_review_at: Some(timestamp),
                last_review_outcome: Some(ProjectReviewOutcome::NoEligiblePr),
                review_last_error: None,
            })
            .await
            .expect("save project");
        store
            .set_setting(SETTING_SCHEMA_VERSION, "11")
            .await
            .expect("mark v11 schema");
        drop(store);

        let reopened = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("reopen");
        assert_eq!(
            reopened
                .get_setting(SETTING_SCHEMA_VERSION)
                .await
                .expect("schema marker")
                .as_deref(),
            Some(SCHEMA_VERSION)
        );
        let projects = reopened.load_projects().await.expect("projects");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, project_id);
        assert_eq!(projects[0].repository_full_name, "owner/repo");
        assert!(projects[0].auto_review_enabled);
        assert_eq!(
            projects[0].reviewer_extra_prompt.as_deref(),
            Some("Focus on safety.")
        );
        assert_eq!(projects[0].review_status, ProjectReviewStatus::Waiting);
        assert_eq!(
            projects[0].last_review_outcome,
            Some(ProjectReviewOutcome::NoEligiblePr)
        );

        let conn = SqliteConnection::open(&db_path).expect("sqlite");
        assert!(!sqlite_column_exists(&conn, "projects", "project_path").expect("column"));
        assert!(!sqlite_column_exists(&conn, "projects", "workspace_path").expect("column"));
    }

    #[tokio::test]
    async fn schema_v12_adds_project_review_columns_without_rebuild() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("config.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open");
        store
            .set_setting(SETTING_SCHEMA_VERSION, "12")
            .await
            .expect("mark v12 schema");
        drop(store);

        let conn = SqliteConnection::open(&db_path).expect("sqlite");
        conn.execute("ALTER TABLE projects DROP COLUMN auto_review_enabled", [])
            .expect("drop auto review column");
        conn.execute("ALTER TABLE projects DROP COLUMN reviewer_extra_prompt", [])
            .expect("drop extra prompt column");
        conn.execute("ALTER TABLE projects DROP COLUMN review_status", [])
            .expect("drop review status column");
        conn.execute(
            "ALTER TABLE projects DROP COLUMN current_reviewer_agent_id",
            [],
        )
        .expect("drop current reviewer column");
        conn.execute(
            "ALTER TABLE projects DROP COLUMN last_review_started_at",
            [],
        )
        .expect("drop started column");
        conn.execute(
            "ALTER TABLE projects DROP COLUMN last_review_finished_at",
            [],
        )
        .expect("drop finished column");
        conn.execute("ALTER TABLE projects DROP COLUMN next_review_at", [])
            .expect("drop next review column");
        conn.execute("ALTER TABLE projects DROP COLUMN last_review_outcome", [])
            .expect("drop outcome column");
        conn.execute("ALTER TABLE projects DROP COLUMN review_last_error", [])
            .expect("drop last error column");
        drop(conn);

        let reopened = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("reopen");
        assert_eq!(
            reopened
                .get_setting(SETTING_SCHEMA_VERSION)
                .await
                .expect("schema marker")
                .as_deref(),
            Some(SCHEMA_VERSION)
        );

        let conn = SqliteConnection::open(&db_path).expect("sqlite");
        for column in [
            "auto_review_enabled",
            "reviewer_extra_prompt",
            "review_status",
            "current_reviewer_agent_id",
            "last_review_started_at",
            "last_review_finished_at",
            "next_review_at",
            "last_review_outcome",
            "review_last_error",
        ] {
            assert!(
                sqlite_column_exists(&conn, "projects", column).expect("column check"),
                "{column} should be added during migration"
            );
        }
        assert!(
            sqlite_table_exists(&conn, "project_review_runs").expect("table check"),
            "project_review_runs should be added during migration"
        );
    }

    #[tokio::test]
    async fn schema_v13_adds_project_review_runs_without_rebuild() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("config.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open");
        store
            .set_setting(SETTING_SCHEMA_VERSION, "13")
            .await
            .expect("mark v13 schema");
        drop(store);

        let conn = SqliteConnection::open(&db_path).expect("sqlite");
        conn.execute("DROP TABLE project_review_runs", [])
            .expect("drop review runs table");
        drop(conn);

        let reopened = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("reopen");
        assert_eq!(
            reopened
                .get_setting(SETTING_SCHEMA_VERSION)
                .await
                .expect("schema marker")
                .as_deref(),
            Some(SCHEMA_VERSION)
        );
        let conn = SqliteConnection::open(&db_path).expect("sqlite");
        assert!(
            sqlite_table_exists(&conn, "project_review_runs").expect("table check"),
            "project_review_runs should be added during v13 -> v14 migration"
        );
        assert!(
            sqlite_table_exists(&conn, "agent_log_entries").expect("table check"),
            "agent_log_entries should be added during v14 -> v15 migration"
        );
        assert!(
            sqlite_table_exists(&conn, "tool_trace_records").expect("table check"),
            "tool_trace_records should be added during v14 -> v15 migration"
        );
    }

    #[tokio::test]
    async fn schema_v15_adds_tool_trace_id_without_rebuild() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("config.sqlite3");
        let config_path = dir.path().join("config.toml");
        let store = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open");
        store
            .set_setting(SETTING_SCHEMA_VERSION, "15")
            .await
            .expect("mark v15 schema");
        drop(store);

        let conn = SqliteConnection::open(&db_path).expect("sqlite");
        conn.execute("DROP TABLE tool_trace_records", [])
            .expect("drop current traces");
        conn.execute(
            "CREATE TABLE tool_trace_records (
                call_id TEXT PRIMARY KEY NOT NULL,
                agent_id TEXT NOT NULL,
                session_id TEXT,
                turn_id TEXT,
                tool_name TEXT NOT NULL,
                arguments_json TEXT NOT NULL DEFAULT '{}',
                output TEXT NOT NULL DEFAULT '',
                success BOOLEAN NOT NULL DEFAULT 0,
                duration_ms BIGINT,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                output_preview TEXT NOT NULL DEFAULT ''
            )",
            [],
        )
        .expect("create old traces");
        conn.execute(
            "INSERT INTO tool_trace_records (
                call_id,
                agent_id,
                session_id,
                turn_id,
                tool_name,
                arguments_json,
                output,
                success,
                duration_ms,
                started_at,
                completed_at,
                output_preview
            ) VALUES (?1, ?2, ?3, ?4, 'container_exec', '{\"command\":\"pwd\"}', '', 1, 1, ?5, ?5, 'pwd')",
            rusqlite::params![
                "call_1",
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                Utc::now().to_rfc3339(),
            ],
        )
        .expect("insert old trace");
        drop(conn);

        let reopened = ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("reopen");
        assert_eq!(
            reopened
                .get_setting(SETTING_SCHEMA_VERSION)
                .await
                .expect("schema marker")
                .as_deref(),
            Some(SCHEMA_VERSION)
        );
        let conn = SqliteConnection::open(&db_path).expect("sqlite");
        assert!(sqlite_column_exists(&conn, "tool_trace_records", "id").expect("column"));
    }

    #[tokio::test]
    async fn mcp_servers_round_trip_json_config() {
        let (_dir, store) = store().await;
        let servers = BTreeMap::from([
            (
                "stdio".to_string(),
                McpServerConfig {
                    scope: McpServerScope::Project,
                    command: Some("demo-mcp".to_string()),
                    args: vec!["--stdio".to_string()],
                    env: BTreeMap::from([("A".to_string(), "B".to_string())]),
                    cwd: Some("/workspace".to_string()),
                    enabled_tools: Some(vec!["echo".to_string()]),
                    disabled_tools: vec!["danger".to_string()],
                    startup_timeout_secs: Some(3),
                    tool_timeout_secs: Some(7),
                    ..Default::default()
                },
            ),
            (
                "http".to_string(),
                McpServerConfig {
                    transport: McpServerTransport::StreamableHttp,
                    url: Some("https://example.com/mcp".to_string()),
                    headers: BTreeMap::from([("X-Test".to_string(), "yes".to_string())]),
                    bearer_token_env: Some("MCP_TOKEN".to_string()),
                    enabled: false,
                    required: true,
                    ..Default::default()
                },
            ),
        ]);

        store.save_mcp_servers(&servers).await.expect("save");
        let loaded = store.list_mcp_servers().await.expect("load");

        assert_eq!(loaded, servers);
        assert_eq!(
            loaded.get("stdio").map(|config| config.scope),
            Some(McpServerScope::Project)
        );
    }
}
