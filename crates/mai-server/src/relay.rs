use futures::{SinkExt, StreamExt};
use mai_protocol::{
    GithubAppInstallationPackagesRequest, GithubAppInstallationStartRequest,
    GithubAppInstallationStartResponse, GithubAppManifestStartRequest,
    GithubAppManifestStartResponse, GithubAppSettingsResponse, GithubInstallationsResponse,
    GithubRepositoriesResponse, GithubRepositorySummary, ProjectId, RelayAck, RelayAckStatus,
    RelayClientHello, RelayEnvelope, RelayError, RelayEvent, RelayEventKind,
    RelayGithubInstallationTokenRequest, RelayGithubInstallationTokenResponse,
    RelayGithubRepositoriesRequest, RelayGithubRepositoryGetRequest,
    RelayGithubRepositoryPackagesRequest, RelayRequest, RelayResponse, RelayStatusResponse,
    RepositoryPackagesResponse, ServiceEventKind,
};
use mai_runtime::{AgentRuntime, RuntimeError};
use mai_runtime::github::GithubAppBackend;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Duration, sleep};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};
use uuid::Uuid;

const RELAY_RPC_TIMEOUT_SECS: u64 = 30;

#[derive(Clone, Debug)]
pub struct RelayClientConfig {
    pub url: String,
    pub token: String,
    pub node_id: String,
}

#[derive(Clone)]
pub struct RelayClient {
    config: RelayClientConfig,
    runtime: Arc<Mutex<Option<Arc<AgentRuntime>>>>,
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
        let connect_url = relay_connect_url(&self.config.url);
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
                _ => continue,
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
                _ => {}
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
        relay_response(request.id, result)
    }

    async fn handle_event(&self, event: RelayEvent) -> RelayAck {
        match self.process_event(&event).await {
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

    async fn process_event(&self, event: &RelayEvent) -> Result<RelayAckStatus, RuntimeError> {
        let runtime = self.runtime.lock().await.clone().ok_or_else(|| {
            RuntimeError::InvalidInput("relay runtime is not attached".to_string())
        })?;
        let event_name = event.kind.as_github_event().to_string();
        let action = event
            .payload
            .get("action")
            .and_then(Value::as_str)
            .map(str::to_string);
        let repository = event.payload.get("repository");
        let repository_full_name = repository
            .and_then(|repo| repo.get("full_name"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let repository_id = repository
            .and_then(|repo| repo.get("id"))
            .and_then(Value::as_u64);
        let installation_id = event
            .payload
            .get("installation")
            .and_then(|installation| installation.get("id"))
            .and_then(Value::as_u64);
        runtime
            .publish_external_event(ServiceEventKind::GithubWebhookReceived {
                delivery_id: event.delivery_id.clone(),
                event: event_name.clone(),
                action: action.clone(),
                repository_full_name: repository_full_name.clone(),
                installation_id,
            })
            .await;

        let Some(project_id) = self
            .runtime
            .lock()
            .await
            .clone()
            .ok_or_else(|| RuntimeError::InvalidInput("relay runtime is not attached".to_string()))?
            .find_project_for_github_event(
                installation_id,
                repository_id,
                repository_full_name.as_deref(),
            )
            .await
        else {
            return Ok(RelayAckStatus::Ignored);
        };

        match event.kind {
            RelayEventKind::PullRequest => {
                if !matches!(
                    action.as_deref(),
                    Some("opened" | "reopened" | "synchronize" | "ready_for_review")
                ) {
                    return Ok(RelayAckStatus::Ignored);
                }
                let Some(pr) = event
                    .payload
                    .get("pull_request")
                    .and_then(|pr| pr.get("number"))
                    .and_then(Value::as_u64)
                    .or_else(|| event.payload.get("number").and_then(Value::as_u64))
                else {
                    return Ok(RelayAckStatus::Ignored);
                };
                self.queue_review(project_id, &event.delivery_id, pr, &event_name)
                    .await?;
                Ok(RelayAckStatus::Processed)
            }
            RelayEventKind::CheckRun | RelayEventKind::CheckSuite => {
                if action.as_deref() != Some("completed") {
                    return Ok(RelayAckStatus::Ignored);
                }
                let prs = associated_pull_requests(&event.payload);
                if prs.is_empty() {
                    return Ok(RelayAckStatus::Ignored);
                }
                let mut processed = false;
                for pr in prs {
                    self.queue_review(project_id, &event.delivery_id, pr, &event_name)
                        .await?;
                    processed = true;
                }
                Ok(if processed {
                    RelayAckStatus::Processed
                } else {
                    RelayAckStatus::Ignored
                })
            }
            RelayEventKind::Push => {
                runtime
                    .handle_project_push_event(project_id, &event.payload)
                    .await?;
                Ok(RelayAckStatus::Processed)
            }
            _ => Ok(RelayAckStatus::Ignored),
        }
    }

    async fn queue_review(
        &self,
        project_id: ProjectId,
        delivery_id: &str,
        pr: u64,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        let runtime = self.runtime.lock().await.clone().ok_or_else(|| {
            RuntimeError::InvalidInput("relay runtime is not attached".to_string())
        })?;
        runtime
            .publish_external_event(ServiceEventKind::ProjectReviewQueued {
                project_id,
                delivery_id: delivery_id.to_string(),
                pr,
                reason: reason.to_string(),
            })
            .await;
        runtime
            .trigger_project_review(project_id, pr, delivery_id.to_string(), reason.to_string())
            .await
    }
}

#[async_trait::async_trait]
impl GithubAppBackend for RelayClient {
    async fn github_app_settings(
        &self,
    ) -> mai_runtime::Result<mai_protocol::GithubAppSettingsResponse> {
        RelayClient::github_app_settings(self).await
    }

    async fn save_github_app_settings(
        &self,
        _request: mai_protocol::GithubAppSettingsRequest,
    ) -> mai_runtime::Result<mai_protocol::GithubAppSettingsResponse> {
        self.github_app_settings().await
    }

    async fn start_github_app_manifest(
        &self,
        request: GithubAppManifestStartRequest,
    ) -> mai_runtime::Result<GithubAppManifestStartResponse> {
        RelayClient::start_github_app_manifest(self, request).await
    }

    async fn complete_github_app_manifest(
        &self,
        _code: &str,
        _state: &str,
    ) -> mai_runtime::Result<mai_protocol::GithubAppSettingsResponse> {
        Err(RuntimeError::InvalidInput(
            "GitHub App manifest callback is handled by mai-relay".to_string(),
        ))
    }

    async fn list_github_installations(&self) -> mai_runtime::Result<GithubInstallationsResponse> {
        RelayClient::list_github_installations(self).await
    }

    async fn refresh_github_installations(
        &self,
    ) -> mai_runtime::Result<GithubInstallationsResponse> {
        RelayClient::list_github_installations(self).await
    }

    async fn list_github_repositories(
        &self,
        installation_id: u64,
    ) -> mai_runtime::Result<GithubRepositoriesResponse> {
        RelayClient::list_github_repositories(self, installation_id).await
    }

    async fn get_github_repository(
        &self,
        installation_id: u64,
        repository_full_name: &str,
    ) -> mai_runtime::Result<GithubRepositorySummary> {
        RelayClient::get_github_repository(self, installation_id, repository_full_name).await
    }

    async fn github_installation_token(
        &self,
        installation_id: u64,
        repository_id: Option<u64>,
        include_packages: bool,
    ) -> mai_runtime::Result<RelayGithubInstallationTokenResponse> {
        RelayClient::create_installation_token(
            self,
            installation_id,
            repository_id,
            include_packages,
        )
        .await
    }
}

fn relay_connect_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let websocket = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        trimmed.to_string()
    };
    if websocket.ends_with("/relay/v1/connect") {
        websocket
    } else {
        format!("{websocket}/relay/v1/connect")
    }
}

fn associated_pull_requests(payload: &Value) -> Vec<u64> {
    let mut prs = HashSet::new();
    for key in ["check_run", "check_suite"] {
        if let Some(items) = payload
            .get(key)
            .and_then(|value| value.get("pull_requests"))
            .and_then(Value::as_array)
        {
            for item in items {
                if let Some(number) = item.get("number").and_then(Value::as_u64) {
                    prs.insert(number);
                }
            }
        }
    }
    prs.into_iter().collect()
}

fn relay_response(id: String, result: Result<Value, RuntimeError>) -> RelayResponse {
    match result {
        Ok(result) => RelayResponse {
            id,
            result: Some(result),
            error: None,
        },
        Err(error) => RelayResponse {
            id,
            result: None,
            error: Some(RelayError {
                code: "runtime".to_string(),
                message: error.to_string(),
            }),
        },
    }
}
