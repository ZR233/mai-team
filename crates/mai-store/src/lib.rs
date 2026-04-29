use chrono::{DateTime, Utc};
use mai_protocol::{
    AgentId, AgentMessage, AgentStatus, AgentSummary, McpServerConfig, MessageRole, ModelInputItem,
    ProviderConfig, ProviderSecret, ProviderSummary, ProvidersConfigRequest, ProvidersResponse,
    ServiceEvent, ServiceEventKind, TokenUsage, TurnId,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    #[error("time parse error: {0}")]
    Time(#[from] chrono::ParseError),
    #[error("invalid config: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub struct ConfigStore {
    path: PathBuf,
    db: Db,
}

#[derive(Debug, Clone)]
pub struct ProviderSelection {
    pub provider: ProviderSecret,
    pub model: String,
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
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
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

        Ok(Self { path, db })
    }

    pub fn default_path() -> Result<PathBuf> {
        let home = dirs::home_dir()
            .ok_or_else(|| StoreError::InvalidConfig("home directory not found".to_string()))?;
        Ok(home.join(".mai-team").join("mai-team.sqlite3"))
    }

    pub fn path(&self) -> &Path {
        &self.path
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
        self.save_providers(ProvidersConfigRequest {
            default_provider_id: Some("openai".to_string()),
            providers: vec![ProviderConfig {
                id: "openai".to_string(),
                name: "OpenAI".to_string(),
                base_url,
                api_key: Some(api_key),
                models: vec![model.clone()],
                default_model: model,
                enabled: true,
            }],
        })
        .await
    }

    pub async fn provider_count(&self) -> Result<usize> {
        let mut db = self.db.clone();
        let count = Query::<List<ProviderRecord>>::all()
            .count()
            .exec(&mut db)
            .await?;
        Ok(count as usize)
    }

    pub async fn providers_response(&self) -> Result<ProvidersResponse> {
        let providers = self.list_provider_secrets().await?;
        let default_provider_id = self.get_setting(SETTING_DEFAULT_PROVIDER_ID).await?;
        Ok(ProvidersResponse {
            providers: providers
                .into_iter()
                .map(|provider| ProviderSummary {
                    id: provider.id,
                    name: provider.name,
                    base_url: provider.base_url,
                    models: provider.models,
                    default_model: provider.default_model,
                    enabled: provider.enabled,
                    has_api_key: !provider.api_key.is_empty(),
                })
                .collect(),
            default_provider_id,
        })
    }

    pub async fn save_providers(&self, request: ProvidersConfigRequest) -> Result<()> {
        validate_provider_request(&request)?;
        let existing_keys = self.existing_provider_keys().await?;
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

        for (index, provider) in request.providers.iter().enumerate() {
            let provider_id = provider.id.trim().to_string();
            let api_key = provider
                .api_key
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
                .or_else(|| existing_keys.get(&provider_id).cloned())
                .unwrap_or_default();
            let models = normalized_models(provider);
            let default_model = normalized_default_model(provider, &models);
            toasty::create!(ProviderRecord {
                id: provider_id.clone(),
                name: provider.name.trim().to_string(),
                base_url: provider.base_url.trim().to_string(),
                api_key,
                default_model,
                enabled: provider.enabled,
                sort_order: index as i64,
            })
            .exec(&mut tx)
            .await?;

            for (model_index, model) in models.iter().enumerate() {
                toasty::create!(ProviderModelRecord {
                    id: child_id(&provider_id, model),
                    provider_id: provider_id.clone(),
                    model: model.clone(),
                    sort_order: model_index as i64,
                })
                .exec(&mut tx)
                .await?;
            }
        }

        if let Some(default_provider_id) = request.default_provider_id.as_deref() {
            set_setting_in_tx(&mut tx, SETTING_DEFAULT_PROVIDER_ID, default_provider_id).await?;
        } else if let Some(first) = request.providers.first() {
            set_setting_in_tx(&mut tx, SETTING_DEFAULT_PROVIDER_ID, first.id.trim()).await?;
        } else {
            delete_setting_in_tx(&mut tx, SETTING_DEFAULT_PROVIDER_ID).await?;
        }

        tx.commit().await?;
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
        let selected_model = model
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| provider.default_model.clone());
        if !provider.models.is_empty() && !provider.models.contains(&selected_model) {
            return Err(StoreError::InvalidConfig(format!(
                "model `{selected_model}` is not configured for provider `{}`",
                provider.id
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
        if let Some(default_provider_id) = self.get_setting(SETTING_DEFAULT_PROVIDER_ID).await?
            && let Some(provider) = providers
                .iter()
                .find(|provider| provider.id == default_provider_id && provider.enabled)
        {
            return Ok(Some(provider.clone()));
        }
        Ok(providers.into_iter().find(|provider| provider.enabled))
    }

    pub async fn list_provider_secrets(&self) -> Result<Vec<ProviderSecret>> {
        let mut db = self.db.clone();
        let mut providers = Query::<List<ProviderRecord>>::all().exec(&mut db).await?;
        providers.sort_by(|left, right| {
            left.sort_order
                .cmp(&right.sort_order)
                .then_with(|| left.name.cmp(&right.name))
        });

        let mut out = Vec::with_capacity(providers.len());
        for provider in providers {
            out.push(ProviderSecret {
                models: self.load_models(&provider.id).await?,
                id: provider.id,
                name: provider.name,
                base_url: provider.base_url,
                api_key: provider.api_key,
                default_model: provider.default_model,
                enabled: provider.enabled,
            });
        }
        Ok(out)
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

    async fn existing_provider_keys(&self) -> Result<HashMap<String, String>> {
        let providers = self.list_provider_secrets().await?;
        Ok(providers
            .into_iter()
            .map(|provider| (provider.id, provider.api_key))
            .collect())
    }

    async fn load_models(&self, provider_id: &str) -> Result<Vec<String>> {
        let mut db = self.db.clone();
        let mut models = Query::<List<ProviderModelRecord>>::filter(
            ProviderModelRecord::fields()
                .provider_id()
                .eq(provider_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        models.sort_by(|left, right| {
            left.sort_order
                .cmp(&right.sort_order)
                .then_with(|| left.model.cmp(&right.model))
        });
        Ok(models.into_iter().map(|row| row.model).collect())
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
        let default_model = normalized_default_model(provider, &models);
        if default_model.is_empty() {
            return Err(StoreError::InvalidConfig(format!(
                "provider `{}` default_model is required",
                provider.id
            )));
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

fn normalized_models(provider: &ProviderConfig) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    for model in &provider.models {
        let model = model.trim();
        if !model.is_empty() && seen.insert(model.to_string()) {
            models.push(model.to_string());
        }
    }
    let default_model = provider.default_model.trim();
    if !default_model.is_empty() && seen.insert(default_model.to_string()) {
        models.insert(0, default_model.to_string());
    }
    models
}

fn normalized_default_model(provider: &ProviderConfig, models: &[String]) -> String {
    if !provider.default_model.trim().is_empty() {
        provider.default_model.trim().to_string()
    } else {
        models.first().cloned().unwrap_or_default()
    }
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
        ServiceEventKind::AgentCreated { agent } => Some(agent.id),
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
    use tempfile::{TempDir, tempdir};

    async fn store() -> (TempDir, ConfigStore) {
        let dir = tempdir().expect("tempdir");
        let store = ConfigStore::open(dir.path().join("config.sqlite3"))
            .await
            .expect("open store");
        (dir, store)
    }

    fn provider(api_key: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            id: "openai".to_string(),
            name: "OpenAI".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: api_key.map(str::to_string),
            models: vec!["gpt-5.2".to_string(), "gpt-5.1".to_string()],
            default_model: "gpt-5.2".to_string(),
            enabled: true,
        }
    }

    #[tokio::test]
    async fn migration_is_idempotent() {
        let (dir, store) = store().await;
        store.migrate().await.expect("migrate twice");
        drop(store);
        ConfigStore::open(dir.path().join("config.sqlite3"))
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
            .resolve_provider(Some("openai"), Some("gpt-5.1"))
            .await
            .expect("resolve");
        assert_eq!(resolved.provider.api_key, "secret");
        assert_eq!(resolved.model, "gpt-5.1");
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
        let store = ConfigStore::open(dir.path().join("config.sqlite3"))
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
        let store = ConfigStore::open(&db_path).await.expect("open");
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

        let reopened = ConfigStore::open(&db_path).await.expect("reopen");
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
        let store = ConfigStore::open(&path).await.expect("rebuild");
        assert_eq!(store.provider_count().await.expect("count"), 0);
    }
}
