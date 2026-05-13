use crate::error::RelayResult;
use crate::store::RelayStore;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use mai_protocol::{RelayEnvelope, RelayResponse};
use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{Mutex, mpsc, oneshot};

#[derive(Debug, Clone)]
pub(crate) struct RelayConfig {
    pub(crate) bind_addr: SocketAddr,
    pub(crate) public_url: String,
    pub(crate) token: String,
    pub(crate) db_path: PathBuf,
    pub(crate) github_api_base_url: String,
    pub(crate) github_web_base_url: String,
}

impl RelayConfig {
    pub(crate) fn from_env(
        default_github_api_base_url: &str,
        default_github_web_base_url: &str,
    ) -> Result<Self> {
        let bind = env::var("MAI_RELAY_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8090".to_string());
        let bind_addr: SocketAddr = bind.parse().context("invalid MAI_RELAY_BIND_ADDR")?;
        let public_url = env::var("MAI_RELAY_PUBLIC_URL")
            .unwrap_or_else(|_| format!("http://127.0.0.1:{}", bind_addr.port()))
            .trim_end_matches('/')
            .to_string();
        let token = env::var("MAI_RELAY_TOKEN").context("MAI_RELAY_TOKEN is required")?;
        let db_path = env::var("MAI_RELAY_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("mai-relay.sqlite3"));
        let github_api_base_url = env::var("GITHUB_API_BASE_URL")
            .unwrap_or_else(|_| default_github_api_base_url.to_string())
            .trim_end_matches('/')
            .to_string();
        let github_web_base_url = env::var("GITHUB_WEB_BASE_URL")
            .unwrap_or_else(|_| default_github_web_base_url.to_string())
            .trim_end_matches('/')
            .to_string();

        Ok(Self {
            bind_addr,
            public_url,
            token,
            db_path,
            github_api_base_url,
            github_web_base_url,
        })
    }
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) store: Arc<RelayStore>,
    pub(crate) token: String,
    pub(crate) public_url: String,
    pub(crate) github_api_base_url: String,
    pub(crate) github_web_base_url: String,
    pub(crate) http: reqwest::Client,
    pub(crate) connection: Arc<Mutex<Option<ActiveConnection>>>,
    pub(crate) pending: Arc<Mutex<HashMap<String, oneshot::Sender<RelayResponse>>>>,
    sequence: Arc<AtomicU64>,
}

impl AppState {
    pub(crate) fn new(
        config: RelayConfig,
        store: Arc<RelayStore>,
        http: reqwest::Client,
    ) -> RelayResult<Self> {
        let sequence = Arc::new(AtomicU64::new(store.next_sequence()?));
        Ok(Self {
            store,
            token: config.token,
            public_url: config.public_url,
            github_api_base_url: config.github_api_base_url,
            github_web_base_url: config.github_web_base_url,
            http,
            connection: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            sequence,
        })
    }

    pub(crate) fn next_delivery_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::SeqCst)
    }
}

#[derive(Clone)]
pub(crate) struct ActiveConnection {
    pub(crate) node_id: String,
    pub(crate) sender: mpsc::UnboundedSender<RelayEnvelope>,
    pub(crate) last_heartbeat_at: DateTime<Utc>,
}
