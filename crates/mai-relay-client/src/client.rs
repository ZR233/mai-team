use std::collections::HashMap;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use mai_protocol::{
    GithubAppInstallationPackagesRequest, GithubAppInstallationStartRequest,
    GithubAppInstallationStartResponse, GithubAppManifestStartRequest,
    GithubAppManifestStartResponse, GithubAppSettingsResponse, GithubInstallationsResponse,
    GithubRepositoriesResponse, GithubRepositorySummary, RelayAck, RelayAckStatus,
    RelayClientHello, RelayEnvelope, RelayEvent,
    RelayGithubInstallationTokenRequest, RelayGithubInstallationTokenResponse,
    RelayGithubRepositoriesRequest, RelayGithubRepositoryGetRequest,
    RelayGithubRepositoryPackagesRequest, RelayRequest, RelayResponse, RelayStatusResponse,
    RepositoryPackagesResponse,
};
use mai_runtime::{AgentRuntime, RuntimeError};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Duration, sleep};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::RelayClientConfig;
use crate::event;
use crate::protocol;

const RELAY_RPC_TIMEOUT_SECS: u64 = 30;

pub struct RelayClient {
    config: RelayClientConfig,
    pub(crate) runtime: Arc<Mutex<Option<Arc<AgentRuntime>>>>,
    state: Arc<Mutex<RelayClientState>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<RelayResponse>>>>,
}

#[derive(Default)]
struct RelayClientState {
    sender: Option<mpsc::UnboundedSender<RelayEnvelope>>,
    connected: bool,
    last_heartbeat_at: Option<chrono::DateTime<chrono::Utc>>,
    message: Option<String>,
}

impl RelayClient {
    pub fn new(config: RelayClientConfig) -> Self {
        Self {
            config,
            runtime: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(RelayClientState::default())),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn set_runtime(&self, runtime: Arc<AgentRuntime>) {
        *self.runtime.lock().await = Some(runtime);
    }

    pub fn start(self: Arc<Self>) {
        tokio::spawn(async move {
            self.run().await;
        });
    }

    pub async fn status(&self) -> RelayStatusResponse {
        let state = self.state.lock().await;
        RelayStatusResponse {
            enabled: true,
            connected: state.connected,
            relay_url: Some(self.config.url.clone()),
            node_id: Some(self.config.node_id.clone()),
            last_heartbeat_at: state.last_heartbeat_at,
            queued_deliveries: None,
            message: state.message.clone(),
        }
    }

    pub async fn request<T, R>(&self, method: &str, params: T) -> Result<R, RuntimeError>
    where
        T: Serialize,
        R: DeserializeOwned,
    {
        let id = Uuid::new_v4().to_string();
        let params = serde_json::to_value(params).map_err(|err| {
            RuntimeError::InvalidInput(format!("relay request serialization failed: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        let sender = {
            let state = self.state.lock().await;
            state.sender.clone()
        }
        .ok_or_else(|| RuntimeError::InvalidInput("relay is not connected".to_string()))?;
        if sender
            .send(RelayEnvelope::Request(RelayRequest {
                id: id.clone(),
                method: method.to_string(),
                params,
            }))
            .is_err()
        {
            self.pending.lock().await.remove(&id);
            return Err(RuntimeError::InvalidInput(
                "relay is not connected".to_string(),
            ));
        }
        let response = tokio::time::timeout(Duration::from_secs(RELAY_RPC_TIMEOUT_SECS), rx)
            .await
            .map_err(|_| RuntimeError::InvalidInput("relay request timed out".to_string()))?
            .map_err(|_| RuntimeError::InvalidInput("relay connection closed".to_string()))?;
        if let Some(error) = response.error {
            return Err(RuntimeError::InvalidInput(format!(
                "relay {} failed: {}",
                error.code, error.message
            )));
        }
        serde_json::from_value(response.result.unwrap_or(Value::Null)).map_err(|err| {
            RuntimeError::InvalidInput(format!("relay response deserialization failed: {err}"))
        })
    }

    pub async fn start_github_app_manifest(
        &self,
        request: GithubAppManifestStartRequest,
    ) -> Result<GithubAppManifestStartResponse, RuntimeError> {
        self.request("github_app_manifest.start", request).await
    }

    pub async fn github_app_settings(&self) -> Result<GithubAppSettingsResponse, RuntimeError> {
        self.request("github.app.get", json!({})).await
    }

    pub async fn start_github_app_installation(
        &self,
        request: GithubAppInstallationStartRequest,
    ) -> Result<GithubAppInstallationStartResponse, RuntimeError> {
        self.request("github.app_installation.start", request).await
    }

    pub async fn list_github_installations(
        &self,
    ) -> Result<GithubInstallationsResponse, RuntimeError> {
        self.request("github.installations.list", json!({})).await
    }

    pub async fn list_github_repositories(
        &self,
        installation_id: u64,
    ) -> Result<GithubRepositoriesResponse, RuntimeError> {
        self.request(
            "github.repositories.list",
            RelayGithubRepositoriesRequest { installation_id },
        )
        .await
    }

    pub async fn get_github_repository(
        &self,
        installation_id: u64,
        repository_full_name: &str,
    ) -> Result<GithubRepositorySummary, RuntimeError> {
        self.request(
            "github.repository.get",
            RelayGithubRepositoryGetRequest {
                installation_id,
                repository_full_name: repository_full_name.to_string(),
            },
        )
        .await
    }

    pub async fn create_installation_token(
        &self,
        installation_id: u64,
        repository_id: Option<u64>,
        include_packages: bool,
    ) -> Result<RelayGithubInstallationTokenResponse, RuntimeError> {
        self.request(
            "github.installation_token.create",
            RelayGithubInstallationTokenRequest {
                installation_id,
                repository_id,
                include_packages,
            },
        )
        .await
    }

    pub async fn list_github_repository_packages(
        &self,
        request: GithubAppInstallationPackagesRequest,
    ) -> Result<RepositoryPackagesResponse, RuntimeError> {
        self.request(
            "github.repository_packages.list",
            RelayGithubRepositoryPackagesRequest {
                installation_id: request.installation_id,
                owner: request.owner,
                repo: request.repo,
            },
        )
        .await
    }

    async fn run(&self) {
        let mut delay = Duration::from_secs(1);
        loop {
            match self.connect_once().await {
                Ok(()) => {
                    delay = Duration::from_secs(1);
                }
                Err(err) => {
                    warn!("relay connection failed: {err}");
                    self.mark_disconnected(Some(err.to_string())).await;
                }
            }
            sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(60));
        }
    }

    async fn connect_once(&self) -> Result<(), RuntimeError> {
        let connect_url = protocol::relay_connect_url(&self.config.url);
        let (stream, _) = connect_async(&connect_url).await.map_err(|err| {
            RuntimeError::InvalidInput(format!("relay websocket connect failed: {err}"))
        })?;
        let (mut writer, mut reader) = stream.split();
        let hello = RelayEnvelope::Hello(RelayClientHello {
            node_id: self.config.node_id.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            token: self.config.token.clone(),
            capabilities: vec!["github-app-relay".to_string()],
        });
        writer
            .send(Message::Text(
                serde_json::to_string(&hello)
                    .map_err(|err| RuntimeError::InvalidInput(err.to_string()))?
                    .into(),
            ))
            .await
            .map_err(|err| RuntimeError::InvalidInput(format!("relay hello failed: {err}")))?;
        let (tx, mut rx) = mpsc::unbounded_channel::<RelayEnvelope>();
        {
            let mut state = self.state.lock().await;
            state.sender = Some(tx.clone());
            state.connected = true;
            state.last_heartbeat_at = Some(chrono::Utc::now());
            state.message = None;
        }
        info!(url = %connect_url, "connected to mai-relay");

        let write_task = tokio::spawn(async move {
            while let Some(envelope) = rx.recv().await {
                match serde_json::to_string(&envelope) {
                    Ok(text) => {
                        if writer.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(err) => warn!("failed to serialize relay envelope: {err}"),
                }
            }
        });

        while let Some(message) = reader.next().await {
            let message = message
                .map_err(|err| RuntimeError::InvalidInput(format!("relay read failed: {err}")))?;
            let text = match message {
                Message::Text(text) => text.to_string(),
                Message::Ping(payload) => {
                    let _ = tx.send(RelayEnvelope::Pong {
                        id: String::from_utf8_lossy(&payload).to_string(),
                    });
                    continue;
                }
                Message::Close(_) => break,
                Message::Binary(_) | Message::Frame(_) | Message::Pong(_) => continue,
            };
            let envelope = serde_json::from_str::<RelayEnvelope>(&text).map_err(|err| {
                RuntimeError::InvalidInput(format!("invalid relay envelope: {err}"))
            })?;
            match envelope {
                RelayEnvelope::Request(request) => {
                    let response = self.handle_request(request).await;
                    let _ = tx.send(RelayEnvelope::Response(response));
                }
                RelayEnvelope::Response(response) => {
                    if let Some(tx) = self.pending.lock().await.remove(&response.id) {
                        let _ = tx.send(response);
                    }
                }
                RelayEnvelope::Event(event) => {
                    let ack = self.handle_event(event).await;
                    let _ = tx.send(RelayEnvelope::Ack(ack));
                }
                RelayEnvelope::Ping { id } => {
                    let _ = tx.send(RelayEnvelope::Pong { id });
                }
                RelayEnvelope::Pong { .. } => {
                    let mut state = self.state.lock().await;
                    state.last_heartbeat_at = Some(chrono::Utc::now());
                }
                RelayEnvelope::Hello(_) | RelayEnvelope::Ack(_) => {}
            }
        }
        write_task.abort();
        self.mark_disconnected(None).await;
        Ok(())
    }

    async fn mark_disconnected(&self, message: Option<String>) {
        let mut state = self.state.lock().await;
        state.sender = None;
        state.connected = false;
        state.message = message;
    }

    async fn handle_request(&self, request: RelayRequest) -> RelayResponse {
        let result: Result<Value, RuntimeError> = match request.method.as_str() {
            "github.webhook_delivery.ack" => {
                let ack = serde_json::from_value::<RelayAck>(request.params).map_err(|err| {
                    RuntimeError::InvalidInput(format!("invalid relay ack request: {err}"))
                });
                match ack {
                    Ok(_) => Ok(json!({ "ok": true })),
                    Err(err) => Err(err),
                }
            }
            other => Err(RuntimeError::InvalidInput(format!(
                "unknown relay server request `{other}`"
            ))),
        };
        protocol::relay_response(request.id, result)
    }

    async fn handle_event(&self, event: RelayEvent) -> RelayAck {
        match event::process_event(self, &event).await {
            Ok(status) => RelayAck {
                delivery_id: event.delivery_id,
                status,
                message: None,
            },
            Err(err) => RelayAck {
                delivery_id: event.delivery_id,
                status: RelayAckStatus::Failed,
                message: Some(err.to_string()),
            },
        }
    }
}
