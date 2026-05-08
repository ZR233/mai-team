use chrono::{DateTime, Utc};
use mai_protocol::{
    AgentConfigRequest, AgentId, AgentMessage, AgentRole, AgentSessionSummary, AgentStatus,
    AgentSummary, ArtifactInfo, GithubAppSettingsRequest, GithubAppSettingsResponse,
    GithubSettingsResponse, McpServerConfig, McpServerTransport, MessageRole, ModelConfig,
    ModelInputItem, ModelReasoningConfig, ModelReasoningVariant, PlanHistoryEntry, PlanStatus,
    ProjectCloneStatus, ProjectId, ProjectStatus, ProjectSummary, ProviderConfig, ProviderKind,
    ProviderPreset, ProviderPresetsResponse, ProviderSecret, ProviderSummary,
    ProvidersConfigRequest, ProvidersResponse, ServiceEvent, ServiceEventKind, SessionId,
    SkillsConfigRequest, TaskId, TaskPlan, TaskReview, TaskStatus, TaskSummary, TokenUsage, TurnId,
    default_true,
};
use rusqlite::Connection as SqliteConnection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use thiserror::Error;
use toasty::Db;
use toasty::stmt::{List, Query};
use toasty_driver_sqlite::Sqlite;
use uuid::Uuid;

const SETTING_AGENT_CONFIG: &str = "agent_config";
const SETTING_SKILLS_CONFIG: &str = "skills_config";
const SETTING_GITHUB_TOKEN: &str = "github_token";
const SETTING_GITHUB_APP_CONFIG: &str = "github_app_config";
const GITHUB_MCP_SERVER_NAME: &str = "github";
const GITHUB_MCP_URL: &str = "https://api.githubcopilot.com/mcp/";
const SETTING_SCHEMA_VERSION: &str = "toasty_schema_version";
const SCHEMA_VERSION: &str = "9";
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
    #[error("invalid config: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub struct ConfigStore {
    path: PathBuf,
    config_path: PathBuf,
    db: Db,
}

#[derive(Debug, Clone)]
pub struct ProviderSelection {
    pub provider: ProviderSecret,
    pub model: ModelConfig,
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

#[derive(Debug, Clone)]
pub struct PersistedTask {
    pub summary: TaskSummary,
    pub plan: TaskPlan,
    pub plan_history: Vec<PlanHistoryEntry>,
    pub reviews: Vec<TaskReview>,
    pub artifacts: Vec<ArtifactInfo>,
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
    repository_id: i64,
    installation_id: i64,
    installation_account: String,
    docker_image: String,
    workspace_path: String,
    clone_status: String,
    maintainer_agent_id: String,
    created_at: String,
    updated_at: String,
    last_error: Option<String>,
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

impl ConfigStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_config_path(path, Self::default_config_path()?).await
    }

    pub async fn open_with_config_path(
        path: impl AsRef<Path>,
        config_path: impl AsRef<Path>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let config_path = config_path.as_ref().to_path_buf();
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
                if current_schema_version.as_deref() == Some("8") {
                    drop(db);
                    migrate_v8_to_v9(&path)?;
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
            db,
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

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
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
        Ok(self.load_providers_toml()?.providers.len())
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
        let file = self.load_providers_toml()?;
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
        if let Some(default_provider_id) = self.load_providers_toml()?.default_provider_id
            && let Some(provider) = providers
                .iter()
                .find(|provider| provider.id == default_provider_id && provider.enabled)
        {
            return Ok(Some(provider.clone()));
        }
        Ok(providers.into_iter().find(|provider| provider.enabled))
    }

    pub async fn list_provider_secrets(&self) -> Result<Vec<ProviderSecret>> {
        let file = self.load_providers_toml()?;
        let mut out = Vec::with_capacity(file.providers.len());
        for (id, provider) in file.providers {
            out.push(ProviderSecret {
                models: provider
                    .models
                    .into_iter()
                    .map(|(id, model)| model.into_model(id))
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

    fn load_providers_toml(&self) -> Result<ProvidersToml> {
        if !self.config_path.exists() {
            return Ok(ProvidersToml::default());
        }
        let text = match std::fs::read_to_string(&self.config_path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProvidersToml::default());
            }
            Err(err) => return Err(err.into()),
        };
        match toml::from_str::<ProvidersToml>(&text) {
            Ok(file) => match validate_providers_toml(&file) {
                Ok(()) => Ok(file),
                Err(_) => {
                    let _ = std::fs::remove_file(&self.config_path);
                    Ok(ProvidersToml::default())
                }
            },
            Err(_) => {
                let _ = std::fs::remove_file(&self.config_path);
                Ok(ProvidersToml::default())
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
        let mut servers = self.list_mcp_servers().await?;
        let mut config = servers.remove(GITHUB_MCP_SERVER_NAME).unwrap_or_default();
        config.transport = McpServerTransport::StreamableHttp;
        config.url = Some(GITHUB_MCP_URL.to_string());
        config.bearer_token = Some(token.to_string());
        config.enabled = true;
        // Rebuild map with "github" first, then remaining servers
        let mut ordered = BTreeMap::new();
        ordered.insert(GITHUB_MCP_SERVER_NAME.to_string(), config);
        ordered.extend(servers);
        self.save_mcp_servers(&ordered).await?;
        Ok(GithubSettingsResponse { has_token: true })
    }

    pub async fn clear_github_token(&self) -> Result<GithubSettingsResponse> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        delete_setting_in_tx(&mut tx, SETTING_GITHUB_TOKEN).await?;
        tx.commit().await?;
        // Only remove the MCP entry if the URL still points to the official GitHub MCP
        let mut servers = self.list_mcp_servers().await?;
        if servers
            .get(GITHUB_MCP_SERVER_NAME)
            .and_then(|c| c.url.as_deref())
            == Some(GITHUB_MCP_URL)
        {
            servers.remove(GITHUB_MCP_SERVER_NAME);
            self.save_mcp_servers(&servers).await?;
        }
        Ok(GithubSettingsResponse { has_token: false })
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
            role: summary.role.map(agent_role_to_str).map(str::to_string),
            name: summary.name.clone(),
            status: agent_status_to_str(&summary.status).to_string(),
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
        Query::<List<ProjectRecordRow>>::filter(
            ProjectRecordRow::fields().id().eq(project.id.to_string()),
        )
        .delete()
        .exec(&mut db)
        .await?;
        toasty::create!(ProjectRecordRow {
            id: project.id.to_string(),
            name: project.name.clone(),
            status: project_status_to_str(&project.status).to_string(),
            owner: project.owner.clone(),
            repo: project.repo.clone(),
            repository_id: u64_to_i64(project.repository_id),
            installation_id: u64_to_i64(project.installation_id),
            installation_account: project.installation_account.clone(),
            docker_image: project.docker_image.clone(),
            workspace_path: project.workspace_path.clone(),
            clone_status: project_clone_status_to_str(&project.clone_status).to_string(),
            maintainer_agent_id: project.maintainer_agent_id.to_string(),
            created_at: project.created_at.to_rfc3339(),
            updated_at: project.updated_at.to_rfc3339(),
            last_error: project.last_error.clone(),
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn delete_project(&self, project_id: ProjectId) -> Result<()> {
        let mut db = self.db.clone();
        Query::<List<ProjectRecordRow>>::filter(
            ProjectRecordRow::fields().id().eq(project_id.to_string()),
        )
        .delete()
        .exec(&mut db)
        .await?;
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
        Query::<List<TaskRecordRow>>::filter(TaskRecordRow::fields().id().eq(task.id.to_string()))
            .delete()
            .exec(&mut db)
            .await?;
        toasty::create!(TaskRecordRow {
            id: task.id.to_string(),
            title: task.title.clone(),
            status: task_status_to_str(&task.status).to_string(),
            planner_agent_id: task.planner_agent_id.to_string(),
            current_agent_id: task.current_agent_id.map(|id| id.to_string()),
            created_at: task.created_at.to_rfc3339(),
            updated_at: task.updated_at.to_rfc3339(),
            last_error: task.last_error.clone(),
            final_report: task.final_report.clone(),
            plan_status: plan_status_to_str(&plan.status).to_string(),
            plan_title: plan.title.clone(),
            plan_markdown: plan.markdown.clone(),
            plan_version: u64_to_i64(plan.version),
            plan_saved_by_agent_id: plan.saved_by_agent_id.map(|id| id.to_string()),
            plan_saved_at: plan.saved_at.map(|time| time.to_rfc3339()),
            plan_approved_at: plan.approved_at.map(|time| time.to_rfc3339()),
            plan_revision_feedback: plan.revision_feedback.clone(),
            plan_revision_requested_at: plan.revision_requested_at.map(|time| time.to_rfc3339()),
        })
        .exec(&mut db)
        .await?;
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

    fn artifacts_dir(&self) -> PathBuf {
        self.path
            .parent()
            .unwrap_or(Path::new("."))
            .join("artifacts")
    }

    pub fn save_artifact(&self, info: &ArtifactInfo) -> Result<()> {
        let dir = self.artifacts_dir();
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(format!("{}.json", info.id));
        let data = serde_json::to_string(info)?;
        std::fs::write(file, data)?;
        Ok(())
    }

    pub fn load_artifacts(&self, task_id: &TaskId) -> Result<Vec<ArtifactInfo>> {
        let dir = self.artifacts_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map_or(true, |ext| ext != "json") {
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
        let dir = self.artifacts_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map_or(true, |ext| ext != "json") {
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
            role: message_role_to_str(&message.role).to_string(),
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
            role: self.role.as_deref().map(parse_agent_role).transpose()?,
            name: self.name,
            status: parse_agent_status(&self.status)?,
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
            status: parse_project_status(&self.status)?,
            owner: self.owner,
            repo: self.repo,
            repository_id: i64_to_u64(self.repository_id),
            installation_id: i64_to_u64(self.installation_id),
            installation_account: self.installation_account,
            docker_image: self.docker_image,
            workspace_path: self.workspace_path,
            clone_status: parse_project_clone_status(&self.clone_status)?,
            maintainer_agent_id: parse_agent_id(&self.maintainer_agent_id)?,
            created_at: parse_utc(&self.created_at)?,
            updated_at: parse_utc(&self.updated_at)?,
            last_error: self.last_error,
        })
    }
}

impl TaskRecordRow {
    fn into_persisted_task(
        self,
        reviews: Vec<TaskReview>,
        plan_history: Vec<PlanHistoryEntry>,
    ) -> Result<PersistedTask> {
        let plan = TaskPlan {
            status: parse_plan_status(&self.plan_status)?,
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
            status: parse_task_status(&self.status)?,
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
            role: parse_message_role(&self.role)?,
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

async fn build_db(path: &Path) -> Result<Db> {
    let mut builder = Db::builder();
    builder.models(toasty::models!(
        McpServerRecord,
        SettingRecord,
        ProjectRecordRow,
        TaskRecordRow,
        TaskReviewRecord,
        PlanHistoryRecord,
        AgentRecordRow,
        AgentSessionRecord,
        AgentMessageRecord,
        AgentHistoryRecord,
        ServiceEventRecord,
    ));
    builder.max_pool_size(1);
    Ok(builder.build(Sqlite::open(path)).await?)
}

fn migrate_v8_to_v9(path: &Path) -> Result<()> {
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
            repository_id BIGINT NOT NULL,
            installation_id BIGINT NOT NULL,
            installation_account TEXT NOT NULL,
            docker_image TEXT NOT NULL,
            workspace_path TEXT NOT NULL,
            clone_status TEXT NOT NULL,
            maintainer_agent_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            last_error TEXT
        )",
        [],
    )?;
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
            reasoning: model.reasoning,
            options: model.options,
            headers: model.headers,
        }
    }

    fn into_model(self, id: String) -> ModelConfig {
        ModelConfig {
            id,
            name: self.name,
            context_tokens: self.context_tokens,
            output_tokens: self.output_tokens,
            supports_tools: self.supports_tools,
            reasoning: self.reasoning,
            options: self.options,
            headers: self.headers,
        }
    }
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
        reasoning: with_reasoning.then(|| mimo_reasoning_config()),
        options: serde_json::Value::Null,
        headers: BTreeMap::new(),
    }
}

fn mimo_context_tokens(id: &str) -> u64 {
    match id {
        "mimo-v2.5-pro" | "mimo-v2-pro" => 256_000,
        "mimo-v2.5" | "mimo-v2-omni" => 128_000,
        "mimo-v2-flash" => 128_000,
        _ => 128_000,
    }
}

fn mimo_output_tokens(id: &str) -> u64 {
    match id {
        "mimo-v2.5-pro" | "mimo-v2-pro" => 131_072,
        "mimo-v2.5" | "mimo-v2-omni" => 32_768,
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

fn fallback_model(id: &str) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens: 128_000,
        output_tokens: 8_192,
        supports_tools: true,
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
                .map(|(model_id, model)| model.clone().into_model(model_id.clone()))
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

fn parse_utc(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

fn agent_status_to_str(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Created => "created",
        AgentStatus::StartingContainer => "starting_container",
        AgentStatus::Idle => "idle",
        AgentStatus::RunningTurn => "running_turn",
        AgentStatus::WaitingTool => "waiting_tool",
        AgentStatus::Completed => "completed",
        AgentStatus::Failed => "failed",
        AgentStatus::Cancelled => "cancelled",
        AgentStatus::DeletingContainer => "deleting_container",
        AgentStatus::Deleted => "deleted",
    }
}

fn parse_agent_status(value: &str) -> Result<AgentStatus> {
    match value {
        "created" => Ok(AgentStatus::Created),
        "starting_container" => Ok(AgentStatus::StartingContainer),
        "idle" => Ok(AgentStatus::Idle),
        "running_turn" => Ok(AgentStatus::RunningTurn),
        "waiting_tool" => Ok(AgentStatus::WaitingTool),
        "completed" => Ok(AgentStatus::Completed),
        "failed" => Ok(AgentStatus::Failed),
        "cancelled" => Ok(AgentStatus::Cancelled),
        "deleting_container" => Ok(AgentStatus::DeletingContainer),
        "deleted" => Ok(AgentStatus::Deleted),
        other => Err(StoreError::InvalidConfig(format!(
            "invalid agent status `{other}`"
        ))),
    }
}

fn task_status_to_str(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Planning => "planning",
        TaskStatus::AwaitingApproval => "awaiting_approval",
        TaskStatus::Executing => "executing",
        TaskStatus::Reviewing => "reviewing",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn parse_task_status(value: &str) -> Result<TaskStatus> {
    match value {
        "planning" => Ok(TaskStatus::Planning),
        "awaiting_approval" => Ok(TaskStatus::AwaitingApproval),
        "executing" => Ok(TaskStatus::Executing),
        "reviewing" => Ok(TaskStatus::Reviewing),
        "completed" => Ok(TaskStatus::Completed),
        "failed" => Ok(TaskStatus::Failed),
        "cancelled" => Ok(TaskStatus::Cancelled),
        other => Err(StoreError::InvalidConfig(format!(
            "invalid task status `{other}`"
        ))),
    }
}

fn project_status_to_str(status: &ProjectStatus) -> &'static str {
    match status {
        ProjectStatus::Creating => "creating",
        ProjectStatus::Ready => "ready",
        ProjectStatus::Failed => "failed",
        ProjectStatus::Deleting => "deleting",
    }
}

fn parse_project_status(value: &str) -> Result<ProjectStatus> {
    match value {
        "creating" => Ok(ProjectStatus::Creating),
        "ready" => Ok(ProjectStatus::Ready),
        "failed" => Ok(ProjectStatus::Failed),
        "deleting" => Ok(ProjectStatus::Deleting),
        other => Err(StoreError::InvalidConfig(format!(
            "invalid project status `{other}`"
        ))),
    }
}

fn project_clone_status_to_str(status: &ProjectCloneStatus) -> &'static str {
    match status {
        ProjectCloneStatus::Pending => "pending",
        ProjectCloneStatus::Cloning => "cloning",
        ProjectCloneStatus::Ready => "ready",
        ProjectCloneStatus::Failed => "failed",
    }
}

fn parse_project_clone_status(value: &str) -> Result<ProjectCloneStatus> {
    match value {
        "pending" => Ok(ProjectCloneStatus::Pending),
        "cloning" => Ok(ProjectCloneStatus::Cloning),
        "ready" => Ok(ProjectCloneStatus::Ready),
        "failed" => Ok(ProjectCloneStatus::Failed),
        other => Err(StoreError::InvalidConfig(format!(
            "invalid project clone status `{other}`"
        ))),
    }
}

fn plan_status_to_str(status: &PlanStatus) -> &'static str {
    match status {
        PlanStatus::Missing => "missing",
        PlanStatus::Ready => "ready",
        PlanStatus::NeedsRevision => "needs_revision",
        PlanStatus::Approved => "approved",
    }
}

fn parse_plan_status(value: &str) -> Result<PlanStatus> {
    match value {
        "missing" => Ok(PlanStatus::Missing),
        "ready" => Ok(PlanStatus::Ready),
        "needs_revision" => Ok(PlanStatus::NeedsRevision),
        "approved" => Ok(PlanStatus::Approved),
        other => Err(StoreError::InvalidConfig(format!(
            "invalid plan status `{other}`"
        ))),
    }
}

fn agent_role_to_str(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Planner => "planner",
        AgentRole::Explorer => "explorer",
        AgentRole::Executor => "executor",
        AgentRole::Reviewer => "reviewer",
    }
}

fn parse_agent_role(value: &str) -> Result<AgentRole> {
    match value {
        "planner" => Ok(AgentRole::Planner),
        "explorer" => Ok(AgentRole::Explorer),
        "executor" => Ok(AgentRole::Executor),
        "reviewer" => Ok(AgentRole::Reviewer),
        other => Err(StoreError::InvalidConfig(format!(
            "invalid agent role `{other}`"
        ))),
    }
}

fn message_role_to_str(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
        MessageRole::Tool => "tool",
    }
}

fn parse_message_role(value: &str) -> Result<MessageRole> {
    match value {
        "user" => Ok(MessageRole::User),
        "assistant" => Ok(MessageRole::Assistant),
        "system" => Ok(MessageRole::System),
        "tool" => Ok(MessageRole::Tool),
        other => Err(StoreError::InvalidConfig(format!(
            "invalid message role `{other}`"
        ))),
    }
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
        AgentStatus, McpServerTransport, MessageRole, ModelContentItem, ModelToolCall,
        ServiceEventKind,
    };
    use serde_json::json;
    use tempfile::{TempDir, tempdir};

    async fn store() -> (TempDir, ConfigStore) {
        let dir = tempdir().expect("tempdir");
        let store = ConfigStore::open_with_config_path(
            dir.path().join("config.sqlite3"),
            dir.path().join("config.toml"),
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
        let mimo_pro = mimo_api
            .models
            .iter()
            .find(|model| model.id == "mimo-v2.5-pro")
            .expect("mimo-v2.5-pro");
        assert_eq!(mimo_pro.output_tokens, 131_072);
        assert!(mimo_pro.reasoning.is_some());
        let mimo_flash = mimo_api
            .models
            .iter()
            .find(|model| model.id == "mimo-v2-flash")
            .expect("mimo-v2-flash");
        assert_eq!(mimo_flash.output_tokens, 65_536);
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
    async fn mcp_servers_round_trip_json_config() {
        let (_dir, store) = store().await;
        let servers = BTreeMap::from([
            (
                "stdio".to_string(),
                McpServerConfig {
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
    }
}
