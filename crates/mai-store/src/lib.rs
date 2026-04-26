use mai_protocol::{
    McpServerConfig, ProviderConfig, ProviderSecret, ProviderSummary, ProvidersConfigRequest,
    ProvidersResponse,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use thiserror::Error;

const SETTING_DEFAULT_PROVIDER_ID: &str = "default_provider_id";
const SETTING_LEGACY_TOML_IMPORTED: &str = "legacy_toml_imported";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("toml error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("store lock poisoned")]
    LockPoisoned,
    #[error("invalid config: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub struct ConfigStore {
    path: PathBuf,
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct ProviderSelection {
    pub provider: ProviderSecret,
    pub model: String,
}

#[derive(Debug, Deserialize)]
struct LegacyMcpFileConfig {
    #[serde(default)]
    mcp_servers: BTreeMap<String, McpServerConfig>,
}

impl ConfigStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        let store = Self {
            path,
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn default_path() -> Result<PathBuf> {
        let home = dirs::home_dir()
            .ok_or_else(|| StoreError::InvalidConfig("home directory not found".to_string()))?;
        Ok(home.join(".mai-team").join("mai-team.sqlite3"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn migrate(&self) -> Result<()> {
        let conn = self.conn()?;
        conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS providers (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                base_url TEXT NOT NULL,
                api_key TEXT NOT NULL DEFAULT '',
                default_model TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                sort_order INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE TABLE IF NOT EXISTS provider_models (
                provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
                model TEXT NOT NULL,
                sort_order INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (provider_id, model)
            );

            CREATE TABLE IF NOT EXISTS mcp_servers (
                name TEXT PRIMARY KEY,
                command TEXT NOT NULL,
                args_json TEXT NOT NULL DEFAULT '[]',
                cwd TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                sort_order INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS mcp_server_env (
                server_name TEXT NOT NULL REFERENCES mcp_servers(name) ON DELETE CASCADE,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (server_name, key)
            );

            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    pub fn seed_default_provider_from_env(
        &self,
        api_key: Option<String>,
        base_url: String,
        model: String,
    ) -> Result<()> {
        if self.provider_count()? > 0 {
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
    }

    pub fn provider_count(&self) -> Result<usize> {
        let conn = self.conn()?;
        Ok(conn.query_row("SELECT COUNT(*) FROM providers", [], |row| {
            row.get::<_, i64>(0)
        })? as usize)
    }

    pub fn providers_response(&self) -> Result<ProvidersResponse> {
        let providers = self.list_provider_secrets()?;
        let default_provider_id = self.get_setting(SETTING_DEFAULT_PROVIDER_ID)?;
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

    pub fn save_providers(&self, request: ProvidersConfigRequest) -> Result<()> {
        validate_provider_request(&request)?;
        let mut conn = self.conn()?;
        let existing_keys = existing_provider_keys(&conn)?;
        let tx = conn.transaction()?;

        tx.execute("DELETE FROM provider_models", [])?;
        tx.execute("DELETE FROM providers", [])?;

        for (index, provider) in request.providers.iter().enumerate() {
            let api_key = provider
                .api_key
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
                .or_else(|| existing_keys.get(&provider.id).cloned())
                .unwrap_or_default();
            let models = normalized_models(provider);
            let default_model = normalized_default_model(provider, &models);
            tx.execute(
                "INSERT INTO providers (id, name, base_url, api_key, default_model, enabled, sort_order)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    provider.id.trim(),
                    provider.name.trim(),
                    provider.base_url.trim(),
                    api_key,
                    default_model,
                    bool_to_i64(provider.enabled),
                    index as i64
                ],
            )?;
            for (model_index, model) in models.iter().enumerate() {
                tx.execute(
                    "INSERT INTO provider_models (provider_id, model, sort_order) VALUES (?1, ?2, ?3)",
                    params![provider.id.trim(), model, model_index as i64],
                )?;
            }
        }

        if let Some(default_provider_id) = request.default_provider_id.as_deref() {
            tx.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                params![SETTING_DEFAULT_PROVIDER_ID, default_provider_id],
            )?;
        } else if let Some(first) = request.providers.first() {
            tx.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                params![SETTING_DEFAULT_PROVIDER_ID, first.id.trim()],
            )?;
        } else {
            tx.execute(
                "DELETE FROM settings WHERE key = ?1",
                params![SETTING_DEFAULT_PROVIDER_ID],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn resolve_provider(
        &self,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<ProviderSelection> {
        let provider = match provider_id.filter(|value| !value.trim().is_empty()) {
            Some(id) => self
                .get_provider_secret(id)?
                .ok_or_else(|| StoreError::InvalidConfig(format!("provider `{id}` not found")))?,
            None => self.default_provider_secret()?.ok_or_else(|| {
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

    pub fn get_provider_secret(&self, id: &str) -> Result<Option<ProviderSecret>> {
        let providers = self.list_provider_secrets()?;
        Ok(providers.into_iter().find(|provider| provider.id == id))
    }

    pub fn default_provider_secret(&self) -> Result<Option<ProviderSecret>> {
        let providers = self.list_provider_secrets()?;
        if providers.is_empty() {
            return Ok(None);
        }
        if let Some(default_provider_id) = self.get_setting(SETTING_DEFAULT_PROVIDER_ID)?
            && let Some(provider) = providers
                .iter()
                .find(|provider| provider.id == default_provider_id && provider.enabled)
        {
            return Ok(Some(provider.clone()));
        }
        Ok(providers.into_iter().find(|provider| provider.enabled))
    }

    pub fn list_provider_secrets(&self) -> Result<Vec<ProviderSecret>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, base_url, api_key, default_model, enabled
             FROM providers ORDER BY sort_order, name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ProviderSecret {
                id: row.get(0)?,
                name: row.get(1)?,
                base_url: row.get(2)?,
                api_key: row.get(3)?,
                default_model: row.get(4)?,
                enabled: row.get::<_, i64>(5)? != 0,
                models: Vec::new(),
            })
        })?;
        let mut providers = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        for provider in &mut providers {
            provider.models = load_models(&conn, &provider.id)?;
        }
        Ok(providers)
    }

    pub fn list_mcp_servers(&self) -> Result<BTreeMap<String, McpServerConfig>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, command, args_json, cwd, enabled FROM mcp_servers ORDER BY sort_order, name",
        )?;
        let rows = stmt.query_map([], |row| {
            let name: String = row.get(0)?;
            let args_json: String = row.get(2)?;
            let args = serde_json::from_str::<Vec<String>>(&args_json).unwrap_or_default();
            Ok((
                name,
                McpServerConfig {
                    command: row.get(1)?,
                    args,
                    env: BTreeMap::new(),
                    cwd: row.get(3)?,
                    enabled: row.get::<_, i64>(4)? != 0,
                },
            ))
        })?;
        let mut servers = BTreeMap::new();
        for row in rows {
            let (name, mut config) = row?;
            config.env = load_mcp_env(&conn, &name)?;
            servers.insert(name, config);
        }
        Ok(servers)
    }

    pub fn save_mcp_servers(&self, servers: &BTreeMap<String, McpServerConfig>) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM mcp_server_env", [])?;
        tx.execute("DELETE FROM mcp_servers", [])?;
        for (index, (name, config)) in servers.iter().enumerate() {
            tx.execute(
                "INSERT INTO mcp_servers (name, command, args_json, cwd, enabled, sort_order)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    name,
                    config.command,
                    serde_json::to_string(&config.args)?,
                    config.cwd,
                    bool_to_i64(config.enabled),
                    index as i64
                ],
            )?;
            for (key, value) in &config.env {
                tx.execute(
                    "INSERT INTO mcp_server_env (server_name, key, value) VALUES (?1, ?2, ?3)",
                    params![name, key, value],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn import_legacy_toml_once(&self, path: impl AsRef<Path>) -> Result<bool> {
        if self.get_setting(SETTING_LEGACY_TOML_IMPORTED)?.as_deref() == Some("1") {
            return Ok(false);
        }
        if !path.as_ref().exists() || !self.list_mcp_servers()?.is_empty() {
            self.set_setting(SETTING_LEGACY_TOML_IMPORTED, "1")?;
            return Ok(false);
        }
        let text = std::fs::read_to_string(path)?;
        let legacy: LegacyMcpFileConfig = toml::from_str(&text)?;
        if legacy.mcp_servers.is_empty() {
            self.set_setting(SETTING_LEGACY_TOML_IMPORTED, "1")?;
            return Ok(false);
        }
        self.save_mcp_servers(&legacy.mcp_servers)?;
        self.set_setting(SETTING_LEGACY_TOML_IMPORTED, "1")?;
        Ok(true)
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn()?;
        Ok(conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| StoreError::LockPoisoned)
    }
}

fn existing_provider_keys(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare("SELECT id, api_key FROM providers")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<HashMap<_, _>, _>>()?)
}

fn load_models(conn: &Connection, provider_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT model FROM provider_models WHERE provider_id = ?1 ORDER BY sort_order, model",
    )?;
    let rows = stmt.query_map(params![provider_id], |row| row.get(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn load_mcp_env(conn: &Connection, server_name: &str) -> Result<BTreeMap<String, String>> {
    let mut stmt =
        conn.prepare("SELECT key, value FROM mcp_server_env WHERE server_name = ?1 ORDER BY key")?;
    let rows = stmt.query_map(params![server_name], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<BTreeMap<_, _>, _>>()?)
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

fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{TempDir, tempdir};

    fn store() -> (TempDir, ConfigStore) {
        let dir = tempdir().expect("tempdir");
        let store = ConfigStore::open(dir.path().join("config.sqlite3")).expect("open store");
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

    #[test]
    fn migration_is_idempotent() {
        let (_dir, store) = store();
        store.migrate().expect("migrate twice");
    }

    #[test]
    fn provider_response_is_redacted_and_preserves_empty_key() {
        let (_dir, store) = store();
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![provider(Some("secret"))],
                default_provider_id: Some("openai".to_string()),
            })
            .expect("save");
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![provider(Some(""))],
                default_provider_id: Some("openai".to_string()),
            })
            .expect("save preserve");

        let response = store.providers_response().expect("providers");
        assert!(response.providers[0].has_api_key);
        let resolved = store
            .resolve_provider(Some("openai"), Some("gpt-5.1"))
            .expect("resolve");
        assert_eq!(resolved.provider.api_key, "secret");
        assert_eq!(resolved.model, "gpt-5.1");
    }

    #[test]
    fn rejects_unknown_model() {
        let (_dir, store) = store();
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![provider(Some("secret"))],
                default_provider_id: Some("openai".to_string()),
            })
            .expect("save");
        assert!(
            store
                .resolve_provider(Some("openai"), Some("unknown"))
                .is_err()
        );
    }

    #[test]
    fn imports_legacy_mcp_config_once() {
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
        let store = ConfigStore::open(dir.path().join("config.sqlite3")).expect("open");
        assert!(store.import_legacy_toml_once(&path).expect("import"));
        assert!(!store.import_legacy_toml_once(&path).expect("skip"));
        let servers = store.list_mcp_servers().expect("servers");
        assert_eq!(servers["demo"].command, "demo-mcp");
        assert_eq!(servers["demo"].env["TOKEN"], "abc");
    }
}
