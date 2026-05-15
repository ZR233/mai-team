use crate::delivery::QueuedDelivery;
use crate::error::{RelayErrorKind, RelayResult};
use crate::github::types::{GithubAppConfig, InstallationState, ManifestState};
use chrono::{DateTime, Utc};
use mai_protocol::{
    GithubAppManifestAccountType, RelayGithubInstallationTokenResponse, RelaySettingsResponse,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::path::PathBuf;

pub(crate) struct RelayStore {
    path: PathBuf,
}

impl RelayStore {
    pub(crate) fn open(path: PathBuf) -> RelayResult<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|err| RelayErrorKind::InvalidInput(err.to_string()))?;
        }
        let store = Self { path };
        store.migrate()?;
        Ok(store)
    }

    fn connection(&self) -> RelayResult<Connection> {
        Ok(Connection::open(&self.path)?)
    }

    fn migrate(&self) -> RelayResult<()> {
        let conn = self.connection()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS manifest_states (
                state TEXT PRIMARY KEY NOT NULL,
                created_at TEXT NOT NULL,
                account_type TEXT NOT NULL,
                org TEXT,
                webhook_secret TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS installation_states (
                state TEXT PRIMARY KEY NOT NULL,
                created_at TEXT NOT NULL,
                origin TEXT NOT NULL,
                return_hash TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS webhook_deliveries (
                delivery_id TEXT PRIMARY KEY NOT NULL,
                sequence INTEGER NOT NULL,
                event_name TEXT NOT NULL,
                payload TEXT NOT NULL,
                received_at TEXT NOT NULL,
                acked_at TEXT
            );
            CREATE TABLE IF NOT EXISTS installation_tokens (
                cache_key TEXT PRIMARY KEY NOT NULL,
                installation_id INTEGER NOT NULL,
                repository_id INTEGER,
                token TEXT NOT NULL,
                expires_at TEXT NOT NULL
            );",
        )?;
        Ok(())
    }

    pub(crate) fn next_sequence(&self) -> RelayResult<u64> {
        let conn = self.connection()?;
        let sequence: Option<i64> = conn
            .query_row("SELECT MAX(sequence) FROM webhook_deliveries", [], |row| {
                row.get(0)
            })
            .optional()?
            .flatten();
        Ok(sequence.unwrap_or(0).saturating_add(1) as u64)
    }

    pub(crate) fn queued_count(&self) -> RelayResult<u64> {
        let conn = self.connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM webhook_deliveries WHERE acked_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    fn set_setting(&self, key: &str, value: &str) -> RelayResult<()> {
        let conn = self.connection()?;
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    fn get_setting(&self, key: &str) -> RelayResult<Option<String>> {
        let conn = self.connection()?;
        Ok(conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub(crate) fn save_github_app_config(&self, config: &GithubAppConfig) -> RelayResult<()> {
        self.set_setting("github_app_config", &serde_json::to_string(config)?)
    }

    pub(crate) fn github_app_config(&self) -> RelayResult<Option<GithubAppConfig>> {
        if let Some(mut config) = crate::github::app::compiled_github_app_config()? {
            if let Some(stored) = self
                .get_setting("github_app_config")?
                .map(|value| serde_json::from_str::<GithubAppConfig>(&value))
                .transpose()?
            {
                config.webhook_secret = stored.webhook_secret;
            }
            return Ok(Some(config));
        }
        self.get_setting("github_app_config")?
            .map(|value| Ok(serde_json::from_str(&value)?))
            .transpose()
    }

    pub(crate) fn save_relay_config(&self, public_url: &str) -> RelayResult<RelaySettingsResponse> {
        let public_url = public_url.trim().trim_end_matches('/').to_string();
        if public_url.is_empty() {
            return Err(RelayErrorKind::InvalidInput(
                "relay public_url is required".to_string(),
            ));
        }
        self.set_setting("relay_public_url", &public_url)?;
        Ok(RelaySettingsResponse {
            enabled: true,
            url: public_url,
            has_token: true,
            node_id: "mai-relay".to_string(),
        })
    }

    pub(crate) fn relay_config(
        &self,
        fallback_public_url: &str,
    ) -> RelayResult<RelaySettingsResponse> {
        let public_url = self
            .get_setting("relay_public_url")?
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback_public_url.trim().trim_end_matches('/').to_string());
        Ok(RelaySettingsResponse {
            enabled: true,
            url: public_url,
            has_token: true,
            node_id: "mai-relay".to_string(),
        })
    }

    pub(crate) fn save_manifest_state(
        &self,
        state: &ManifestState,
        webhook_secret: &str,
    ) -> RelayResult<()> {
        let conn = self.connection()?;
        conn.execute(
            "INSERT OR REPLACE INTO manifest_states
             (state, created_at, account_type, org, webhook_secret)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                state.state,
                state.created_at.to_rfc3339(),
                match state.account_type {
                    GithubAppManifestAccountType::Personal => "personal",
                    GithubAppManifestAccountType::Organization => "organization",
                },
                state.org,
                webhook_secret,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn take_manifest_state(&self, state: &str) -> RelayResult<(ManifestState, String)> {
        let conn = self.connection()?;
        let row = conn
            .query_row(
                "SELECT state, created_at, account_type, org, webhook_secret
                 FROM manifest_states WHERE state = ?1",
                [state],
                |row| {
                    let account_type: String = row.get(2)?;
                    let account_type = if account_type == "organization" {
                        GithubAppManifestAccountType::Organization
                    } else {
                        GithubAppManifestAccountType::Personal
                    };
                    let created_at: String = row.get(1)?;
                    Ok((
                        ManifestState {
                            state: row.get(0)?,
                            created_at: DateTime::parse_from_rfc3339(&created_at)
                                .map(|time| time.with_timezone(&Utc))
                                .unwrap_or_else(|_| Utc::now()),
                            account_type,
                            org: row.get(3)?,
                        },
                        row.get(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| RelayErrorKind::InvalidInput("manifest state not found".to_string()))?;
        conn.execute("DELETE FROM manifest_states WHERE state = ?1", [state])?;
        Ok(row)
    }

    pub(crate) fn save_installation_state(&self, state: &InstallationState) -> RelayResult<()> {
        let conn = self.connection()?;
        conn.execute(
            "INSERT OR REPLACE INTO installation_states
             (state, created_at, origin, return_hash)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                state.state,
                state.created_at.to_rfc3339(),
                state.origin,
                state.return_hash,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn take_installation_state(&self, state: &str) -> RelayResult<InstallationState> {
        let conn = self.connection()?;
        let row = conn
            .query_row(
                "SELECT state, created_at, origin, return_hash
                 FROM installation_states WHERE state = ?1",
                [state],
                |row| {
                    let created_at: String = row.get(1)?;
                    Ok(InstallationState {
                        state: row.get(0)?,
                        created_at: DateTime::parse_from_rfc3339(&created_at)
                            .map(|time| time.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        origin: row.get(2)?,
                        return_hash: row.get(3)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| {
                RelayErrorKind::InvalidInput("installation state not found".to_string())
            })?;
        conn.execute("DELETE FROM installation_states WHERE state = ?1", [state])?;
        Ok(row)
    }

    pub(crate) fn take_latest_installation_state(&self) -> RelayResult<InstallationState> {
        let conn = self.connection()?;
        let row = conn
            .query_row(
                "SELECT state, created_at, origin, return_hash
                 FROM installation_states
                 ORDER BY created_at DESC
                 LIMIT 1",
                [],
                |row| {
                    let created_at: String = row.get(1)?;
                    Ok(InstallationState {
                        state: row.get(0)?,
                        created_at: DateTime::parse_from_rfc3339(&created_at)
                            .map(|time| time.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        origin: row.get(2)?,
                        return_hash: row.get(3)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| {
                RelayErrorKind::InvalidInput("installation state not found".to_string())
            })?;
        conn.execute(
            "DELETE FROM installation_states WHERE state = ?1",
            [&row.state],
        )?;
        Ok(row)
    }

    pub(crate) fn insert_delivery(
        &self,
        sequence: u64,
        delivery_id: &str,
        event_name: &str,
        payload: &Value,
    ) -> RelayResult<bool> {
        let conn = self.connection()?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO webhook_deliveries
             (delivery_id, sequence, event_name, payload, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                delivery_id,
                sequence as i64,
                event_name,
                serde_json::to_string(payload)?,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(inserted > 0)
    }

    pub(crate) fn list_unacked_deliveries(&self) -> RelayResult<Vec<QueuedDelivery>> {
        let conn = self.connection()?;
        let mut statement = conn.prepare(
            "SELECT sequence, delivery_id, event_name, payload
             FROM webhook_deliveries
             WHERE acked_at IS NULL
             ORDER BY sequence ASC
             LIMIT 500",
        )?;
        let rows = statement.query_map([], |row| {
            let payload: String = row.get(3)?;
            Ok(QueuedDelivery {
                sequence: row.get::<_, i64>(0)?.max(0) as u64,
                delivery_id: row.get(1)?,
                event_name: row.get(2)?,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            })
        })?;
        let mut deliveries = Vec::new();
        for row in rows {
            deliveries.push(row?);
        }
        Ok(deliveries)
    }

    pub(crate) fn ack_delivery(&self, delivery_id: &str) -> RelayResult<()> {
        let conn = self.connection()?;
        conn.execute(
            "UPDATE webhook_deliveries SET acked_at = ?2 WHERE delivery_id = ?1",
            params![delivery_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub(crate) fn cached_token(
        &self,
        installation_id: u64,
        repository_id: Option<u64>,
        include_packages: bool,
    ) -> RelayResult<Option<RelayGithubInstallationTokenResponse>> {
        let conn = self.connection()?;
        let cache_key = token_cache_key(installation_id, repository_id, include_packages);
        let token = conn
            .query_row(
                "SELECT token, expires_at FROM installation_tokens WHERE cache_key = ?1",
                [cache_key],
                |row| {
                    let expires_at: String = row.get(1)?;
                    Ok(RelayGithubInstallationTokenResponse {
                        token: row.get(0)?,
                        expires_at: DateTime::parse_from_rfc3339(&expires_at)
                            .map(|time| time.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                    })
                },
            )
            .optional()?;
        Ok(token)
    }

    pub(crate) fn save_cached_token(
        &self,
        installation_id: u64,
        repository_id: Option<u64>,
        include_packages: bool,
        token: &RelayGithubInstallationTokenResponse,
    ) -> RelayResult<()> {
        let conn = self.connection()?;
        conn.execute(
            "INSERT INTO installation_tokens
             (cache_key, installation_id, repository_id, token, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(cache_key) DO UPDATE SET
                token = excluded.token,
                expires_at = excluded.expires_at",
            params![
                token_cache_key(installation_id, repository_id, include_packages),
                installation_id as i64,
                repository_id.map(|id| id as i64),
                token.token,
                token.expires_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }
}

fn token_cache_key(
    installation_id: u64,
    repository_id: Option<u64>,
    include_packages: bool,
) -> String {
    let scope = if include_packages {
        "packages"
    } else {
        "default"
    };
    format!(
        "{installation_id}:{scope}:{}",
        repository_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "all".to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn delivery_queue_dedupes_and_acks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = RelayStore::open(dir.path().join("relay.sqlite3")).expect("store");
        let payload = json!({ "action": "opened" });

        assert!(
            store
                .insert_delivery(1, "delivery-1", "pull_request", &payload)
                .expect("insert")
        );
        assert!(
            !store
                .insert_delivery(2, "delivery-1", "pull_request", &payload)
                .expect("dedupe")
        );
        assert_eq!(store.list_unacked_deliveries().expect("list").len(), 1);
        store.ack_delivery("delivery-1").expect("ack");
        assert!(store.list_unacked_deliveries().expect("list").is_empty());
    }

    #[test]
    fn installation_state_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = RelayStore::open(dir.path().join("relay.sqlite3")).expect("store");
        store
            .save_installation_state(&InstallationState {
                state: "state-1".to_string(),
                created_at: Utc::now(),
                origin: "http://127.0.0.1:8080".to_string(),
                return_hash: "#projects".to_string(),
            })
            .expect("save");

        let state = store.take_installation_state("state-1").expect("take");
        assert_eq!(state.origin, "http://127.0.0.1:8080");
        assert_eq!(state.return_hash, "#projects");
        assert!(store.take_installation_state("state-1").is_err());
    }

    #[test]
    fn latest_installation_state_supports_github_callback_without_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = RelayStore::open(dir.path().join("relay.sqlite3")).expect("store");
        store
            .save_installation_state(&InstallationState {
                state: "state-older".to_string(),
                created_at: DateTime::parse_from_rfc3339("2026-05-13T01:00:00Z")
                    .expect("time")
                    .with_timezone(&Utc),
                origin: "http://127.0.0.1:8080".to_string(),
                return_hash: "#projects".to_string(),
            })
            .expect("save older");
        store
            .save_installation_state(&InstallationState {
                state: "state-newer".to_string(),
                created_at: DateTime::parse_from_rfc3339("2026-05-13T02:00:00Z")
                    .expect("time")
                    .with_timezone(&Utc),
                origin: "http://127.0.0.1:8080".to_string(),
                return_hash: "#projects".to_string(),
            })
            .expect("save newer");

        let state = store.take_latest_installation_state().expect("latest");
        assert_eq!(state.state, "state-newer");
        assert!(store.take_installation_state("state-newer").is_err());
        assert!(store.take_installation_state("state-older").is_ok());
    }

    #[test]
    fn relay_config_round_trips_public_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = RelayStore::open(dir.path().join("relay.sqlite3")).expect("store");

        let default = store
            .relay_config("https://fallback.example/")
            .expect("default relay config");
        assert_eq!(default.url, "https://fallback.example");
        assert!(default.enabled);
        assert!(default.has_token);

        let saved = store
            .save_relay_config(" https://relay.example/ ")
            .expect("save relay config");
        assert_eq!(saved.url, "https://relay.example");

        let loaded = store
            .relay_config("https://fallback.example")
            .expect("load relay config");
        assert_eq!(loaded, saved);
    }

    #[cfg(feature = "compiled-github-app-config")]
    #[test]
    fn compiled_github_app_config_ignores_persisted_app_fields() {
        let Some(compiled_app_id) = option_env!("MAI_RELAY_GITHUB_APP_ID") else {
            return;
        };
        let Some(compiled_private_key) = option_env!("MAI_RELAY_GITHUB_APP_PRIVATE_KEY") else {
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let store = RelayStore::open(dir.path().join("relay.sqlite3")).expect("store");
        store
            .save_github_app_config(&GithubAppConfig {
                app_id: "persisted-app-id".to_string(),
                private_key: "persisted-private-key".to_string(),
                webhook_secret: "persisted-webhook-secret".to_string(),
                app_slug: Some("persisted-slug".to_string()),
                app_html_url: Some("https://github.com/apps/persisted".to_string()),
                owner_login: Some("persisted-owner".to_string()),
                owner_type: Some("Organization".to_string()),
            })
            .expect("save persisted app config");

        let loaded = store
            .github_app_config()
            .expect("load app config")
            .expect("compiled config");

        assert_eq!(loaded.app_id, compiled_app_id);
        assert_eq!(loaded.private_key, compiled_private_key);
        assert_eq!(loaded.webhook_secret, "persisted-webhook-secret");
    }
}
