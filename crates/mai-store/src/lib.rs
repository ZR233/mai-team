use chrono::{DateTime, Utc};
use mai_protocol::{
    AgentId, AgentMessage, AgentStatus, AgentSummary, McpServerConfig, MessageRole, ModelConfig,
    ModelInputItem, ProviderConfig, ProviderKind, ProviderPreset, ProviderPresetsResponse,
    ProviderSecret, ProviderSummary, ProvidersConfigRequest, ProvidersResponse, ReasoningEffort,
    ServiceEvent, ServiceEventKind, TokenUsage, TurnId, default_true,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use thiserror::Error;
use toasty::Db;
use toasty::stmt::{List, Query};
use toasty_driver_sqlite::Sqlite;
use uuid::Uuid;

const SETTING_DEFAULT_PROVIDER_ID: &str = "default_provider_id";
const SETTING_LEGACY_TOML_IMPORTED: &str = "legacy_toml_imported";
const SETTING_SCHEMA_VERSION: &str = "toasty_schema_version";
const SCHEMA_VERSION: &str = "1";
const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("toasty error: {0}")]
    Toasty(#[from] toasty::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
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
    pub messages: Vec<AgentMessage>,
    pub history: Vec<ModelInputItem>,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RuntimeSnapshot {
    pub agents: Vec<PersistedAgent>,
    pub recent_events: Vec<ServiceEvent>,
    pub next_sequence: u64,
}

#[derive(Debug, Deserialize)]
struct LegacyMcpFileConfig {
    #[serde(default)]
    mcp_servers: BTreeMap<String, McpServerConfig>,
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
    supports_reasoning: bool,
    #[serde(default)]
    reasoning_efforts: Vec<ReasoningEffort>,
    #[serde(default)]
    default_reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "is_null")]
    options: serde_json::Value,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "providers"]
struct ProviderRecord {
    #[key]
    id: String,
    name: String,
    base_url: String,
    api_key: String,
    default_model: String,
    enabled: bool,
    sort_order: i64,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "provider_models"]
struct ProviderModelRecord {
    #[key]
    id: String,
    #[index]
    provider_id: String,
    model: String,
    sort_order: i64,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "mcp_servers"]
struct McpServerRecord {
    #[key]
    name: String,
    command: String,
    args_json: String,
    cwd: Option<String>,
    enabled: bool,
    sort_order: i64,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "mcp_server_env"]
struct McpServerEnvRecord {
    #[key]
    id: String,
    #[index]
    server_name: String,
    key: String,
    value: String,
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
    name: String,
    status: String,
    container_id: Option<String>,
    provider_id: String,
    provider_name: String,
    model: String,
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
#[table = "agent_messages"]
struct AgentMessageRecord {
    #[key]
    id: String,
    #[index]
    agent_id: String,
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
        } else if get_setting_on(&db, SETTING_SCHEMA_VERSION)
            .await
            .ok()
            .flatten()
            .as_deref()
            != Some(SCHEMA_VERSION)
        {
            drop(db);
            let _ = std::fs::remove_file(&path);
            db = build_db(&path).await?;
            db.push_schema().await?;
            set_setting_on(&mut db, SETTING_SCHEMA_VERSION, SCHEMA_VERSION).await?;
        }

        let store = Self {
            path,
            config_path,
            db,
        };
        store.clear_legacy_provider_storage().await?;
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

    pub async fn migrate(&self) -> Result<()> {
        Ok(())
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
            Ok(file) => Ok(file),
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

    async fn clear_legacy_provider_storage(&self) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<ProviderModelRecord>>::all()
            .delete()
            .exec(&mut tx)
            .await?;
        Query::<List<ProviderRecord>>::all()
            .delete()
            .exec(&mut tx)
            .await?;
        delete_setting_in_tx(&mut tx, SETTING_DEFAULT_PROVIDER_ID).await?;
        tx.commit().await?;
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
            let args = serde_json::from_str::<Vec<String>>(&row.args_json).unwrap_or_default();
            servers.insert(
                row.name.clone(),
                McpServerConfig {
                    command: row.command,
                    args,
                    env: self.load_mcp_env(&row.name).await?,
                    cwd: row.cwd,
                    enabled: row.enabled,
                },
            );
        }
        Ok(servers)
    }

    pub async fn save_mcp_servers(
        &self,
        servers: &BTreeMap<String, McpServerConfig>,
    ) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<McpServerEnvRecord>>::all()
            .delete()
            .exec(&mut tx)
            .await?;
        Query::<List<McpServerRecord>>::all()
            .delete()
            .exec(&mut tx)
            .await?;

        for (index, (name, config)) in servers.iter().enumerate() {
            toasty::create!(McpServerRecord {
                name: name.clone(),
                command: config.command.clone(),
                args_json: serde_json::to_string(&config.args)?,
                cwd: config.cwd.clone(),
                enabled: config.enabled,
                sort_order: index as i64,
            })
            .exec(&mut tx)
            .await?;

            for (key, value) in &config.env {
                toasty::create!(McpServerEnvRecord {
                    id: child_id(name, key),
                    server_name: name.clone(),
                    key: key.clone(),
                    value: value.clone(),
                })
                .exec(&mut tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }

    pub async fn import_legacy_toml_once(&self, path: impl AsRef<Path>) -> Result<bool> {
        if self
            .get_setting(SETTING_LEGACY_TOML_IMPORTED)
            .await?
            .as_deref()
            == Some("1")
        {
            return Ok(false);
        }
        if !path.as_ref().exists() || !self.list_mcp_servers().await?.is_empty() {
            self.set_setting(SETTING_LEGACY_TOML_IMPORTED, "1").await?;
            return Ok(false);
        }
        let text = std::fs::read_to_string(path)?;
        let legacy: LegacyMcpFileConfig = toml::from_str(&text)?;
        if legacy.mcp_servers.is_empty() {
            self.set_setting(SETTING_LEGACY_TOML_IMPORTED, "1").await?;
            return Ok(false);
        }
        self.save_mcp_servers(&legacy.mcp_servers).await?;
        self.set_setting(SETTING_LEGACY_TOML_IMPORTED, "1").await?;
        Ok(true)
    }

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        get_setting_on(&self.db, key).await
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let mut db = self.db.clone();
        set_setting_on(&mut db, key, value).await
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
            name: summary.name.clone(),
            status: agent_status_to_str(&summary.status).to_string(),
            container_id: summary.container_id.clone(),
            provider_id: summary.provider_id.clone(),
            provider_name: summary.provider_name.clone(),
            model: summary.model.clone(),
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

    pub async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        delete_agent_row_in_tx(&mut tx, agent_id).await?;
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

    pub async fn append_agent_message(
        &self,
        agent_id: AgentId,
        position: usize,
        message: &AgentMessage,
    ) -> Result<()> {
        let mut db = self.db.clone();
        toasty::create!(AgentMessageRecord {
            id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
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
        position: usize,
        item: &ModelInputItem,
    ) -> Result<()> {
        let mut db = self.db.clone();
        toasty::create!(AgentHistoryRecord {
            id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            position: position as i64,
            item_json: serde_json::to_string(item)?,
        })
        .exec(&mut db)
        .await?;
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
            let messages = self.load_agent_messages(agent_id).await?;
            let history = self.load_agent_history(agent_id).await?;
            let system_prompt = row.system_prompt.clone();
            agents.push(PersistedAgent {
                summary: row.into_summary()?,
                messages,
                history,
                system_prompt,
            });
        }

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
            recent_events,
            next_sequence,
        })
    }

    async fn load_mcp_env(&self, server_name: &str) -> Result<BTreeMap<String, String>> {
        let mut db = self.db.clone();
        let mut env = Query::<List<McpServerEnvRecord>>::filter(
            McpServerEnvRecord::fields()
                .server_name()
                .eq(server_name.to_string()),
        )
        .exec(&mut db)
        .await?;
        env.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(env.into_iter().map(|row| (row.key, row.value)).collect())
    }

    async fn load_agent_messages(&self, agent_id: AgentId) -> Result<Vec<AgentMessage>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<AgentMessageRecord>>::filter(
            AgentMessageRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.sort_by_key(|row| row.position);
        rows.into_iter()
            .map(AgentMessageRecord::into_message)
            .collect()
    }

    async fn load_agent_history(&self, agent_id: AgentId) -> Result<Vec<ModelInputItem>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<AgentHistoryRecord>>::filter(
            AgentHistoryRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.sort_by_key(|row| row.position);
        rows.into_iter()
            .map(|row| serde_json::from_str::<ModelInputItem>(&row.item_json).map_err(Into::into))
            .collect()
    }
}

impl AgentRecordRow {
    fn into_summary(self) -> Result<AgentSummary> {
        Ok(AgentSummary {
            id: parse_agent_id(&self.id)?,
            parent_id: self.parent_id.as_deref().map(parse_agent_id).transpose()?,
            name: self.name,
            status: parse_agent_status(&self.status)?,
            container_id: self.container_id,
            provider_id: self.provider_id,
            provider_name: self.provider_name,
            model: self.model,
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

impl AgentMessageRecord {
    fn into_message(self) -> Result<AgentMessage> {
        Ok(AgentMessage {
            role: parse_message_role(&self.role)?,
            content: self.content,
            created_at: parse_utc(&self.created_at)?,
        })
    }
}

async fn build_db(path: &Path) -> Result<Db> {
    let mut builder = Db::builder();
    builder.models(toasty::models!(
        ProviderRecord,
        ProviderModelRecord,
        McpServerRecord,
        McpServerEnvRecord,
        SettingRecord,
        AgentRecordRow,
        AgentMessageRecord,
        AgentHistoryRecord,
        ServiceEventRecord,
    ));
    builder.max_pool_size(1);
    Ok(builder.build(Sqlite::open(path)).await?)
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
            supports_reasoning: model.supports_reasoning,
            reasoning_efforts: model.reasoning_efforts,
            default_reasoning_effort: model.default_reasoning_effort,
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
            supports_reasoning: self.supports_reasoning,
            reasoning_efforts: self.reasoning_efforts,
            default_reasoning_effort: self.default_reasoning_effort,
            options: self.options,
            headers: self.headers,
        }
    }
}

fn provider_preset(kind: ProviderKind) -> ProviderPreset {
    let provider = builtin_provider(kind);
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
                    supports_reasoning: false,
                    reasoning_efforts: Vec::new(),
                    default_reasoning_effort: None,
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
                deepseek_model("deepseek-v4-pro", false),
                deepseek_model("deepseek-chat", false),
                deepseek_model("deepseek-reasoner", true),
            ],
        },
    }
}

fn openai_reasoning_model(id: &str, context_tokens: u64, output_tokens: u64) -> ModelConfig {
    let mut efforts = vec![
        ReasoningEffort::Minimal,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
    ];
    if id.contains("5.4") || id.contains("5.5") {
        efforts.push(ReasoningEffort::Xhigh);
    }
    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens,
        output_tokens,
        supports_tools: true,
        supports_reasoning: true,
        reasoning_efforts: efforts,
        default_reasoning_effort: Some(ReasoningEffort::Medium),
        options: serde_json::Value::Null,
        headers: BTreeMap::new(),
    }
}

fn deepseek_model(id: &str, supports_reasoning: bool) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens: 128_000,
        output_tokens: 8_192,
        supports_tools: true,
        supports_reasoning,
        reasoning_efforts: supports_reasoning
            .then(|| {
                vec![
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                ]
            })
            .unwrap_or_default(),
        default_reasoning_effort: supports_reasoning.then_some(ReasoningEffort::Medium),
        options: serde_json::Value::Null,
        headers: BTreeMap::new(),
    }
}

fn fallback_model(id: &str) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens: 128_000,
        output_tokens: 8_192,
        supports_tools: true,
        supports_reasoning: false,
        reasoning_efforts: Vec::new(),
        default_reasoning_effort: None,
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
            if model.supports_reasoning {
                if model.reasoning_efforts.is_empty() {
                    return Err(StoreError::InvalidConfig(format!(
                        "reasoning model `{}` must configure reasoning_efforts",
                        model.id
                    )));
                }
                if let Some(default_effort) = model.default_reasoning_effort
                    && !model.reasoning_efforts.contains(&default_effort)
                {
                    return Err(StoreError::InvalidConfig(format!(
                        "model `{}` default_reasoning_effort is not in reasoning_efforts",
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

fn child_id(parent: &str, child: &str) -> String {
    format!("{parent}\u{1f}{child}")
}

fn parse_agent_id(value: &str) -> Result<AgentId> {
    Uuid::parse_str(value)
        .map_err(|err| StoreError::InvalidConfig(format!("invalid agent id `{value}`: {err}")))
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
        | ServiceEventKind::AgentMessage { agent_id, .. } => Some(*agent_id),
        ServiceEventKind::Error { agent_id, .. } => *agent_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{AgentStatus, MessageRole, ModelContentItem, ServiceEventKind};
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
            supports_reasoning: true,
            reasoning_efforts: vec![
                ReasoningEffort::Minimal,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ],
            default_reasoning_effort: Some(ReasoningEffort::Medium),
            options: serde_json::Value::Null,
            headers: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn migration_is_idempotent() {
        let (dir, store) = store().await;
        store.migrate().await.expect("migrate twice");
        drop(store);
        ConfigStore::open_with_config_path(
            dir.path().join("config.sqlite3"),
            dir.path().join("config.toml"),
        )
        .await
        .expect("reopen existing store");
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
        assert!(
            deepseek
                .models
                .iter()
                .any(|model| model.id == "deepseek-reasoner")
        );
    }

    #[tokio::test]
    async fn provider_toml_preserves_custom_model_metadata() {
        let (_dir, store) = store().await;
        let mut provider = provider(Some("secret"));
        let mut custom = test_model("custom-chat");
        custom.context_tokens = 123_456;
        custom.output_tokens = 4_096;
        custom.supports_tools = false;
        custom.supports_reasoning = false;
        custom.reasoning_efforts = Vec::new();
        custom.default_reasoning_effort = None;
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
    async fn old_provider_toml_schema_is_cleaned() {
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
    async fn imports_legacy_mcp_config_once() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
                [mcp_servers.demo]
                command = "demo-mcp"
                args = ["--stdio"]
                enabled = true

                [mcp_servers.demo.env]
                TOKEN = "abc"
            "#,
        )
        .expect("write legacy");
        let store = ConfigStore::open_with_config_path(
            dir.path().join("config.sqlite3"),
            dir.path().join("providers.toml"),
        )
        .await
        .expect("open");
        assert!(store.import_legacy_toml_once(&path).await.expect("import"));
        assert!(!store.import_legacy_toml_once(&path).await.expect("skip"));
        let servers = store.list_mcp_servers().await.expect("servers");
        assert_eq!(servers["demo"].command, "demo-mcp");
        assert_eq!(servers["demo"].env["TOKEN"], "abc");
    }

    #[tokio::test]
    async fn runtime_snapshot_survives_reopen() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("config.sqlite3");
        let store = ConfigStore::open_with_config_path(&db_path, dir.path().join("config.toml"))
            .await
            .expect("open");
        let agent_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        let now = Utc::now();
        let summary = AgentSummary {
            id: agent_id,
            parent_id: None,
            name: "agent-test".to_string(),
            status: AgentStatus::Completed,
            container_id: Some("container".to_string()),
            provider_id: "openai".to_string(),
            provider_name: "OpenAI".to_string(),
            model: "gpt-5.2".to_string(),
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
        let message = AgentMessage {
            role: MessageRole::User,
            content: "hello".to_string(),
            created_at: now,
        };
        let history = ModelInputItem::Message {
            role: "user".to_string(),
            content: vec![ModelContentItem::InputText {
                text: "hello".to_string(),
            }],
        };
        let event = ServiceEvent {
            sequence: 7,
            timestamp: now,
            kind: ServiceEventKind::AgentMessage {
                agent_id,
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
            .append_agent_message(agent_id, 0, &message)
            .await
            .expect("message");
        store
            .append_agent_history_item(agent_id, 0, &history)
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
        assert_eq!(snapshot.agents[0].system_prompt.as_deref(), Some("system"));
        assert_eq!(snapshot.agents[0].messages[0].content, "hello");
        assert_eq!(snapshot.agents[0].history.len(), 1);
        assert_eq!(snapshot.recent_events.len(), 1);
    }

    #[tokio::test]
    async fn old_schema_without_marker_is_rebuilt() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.sqlite3");
        std::fs::write(&path, b"not sqlite").expect("write invalid old db");
        let store = ConfigStore::open_with_config_path(&path, dir.path().join("config.toml"))
            .await
            .expect("rebuild");
        assert_eq!(store.provider_count().await.expect("count"), 0);
    }
}
