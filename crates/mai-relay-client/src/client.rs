use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use mai_protocol::{
    GithubAppInstallationPackagesRequest, GithubAppInstallationStartRequest,
    GithubAppInstallationStartResponse, GithubAppManifestStartRequest,
    GithubAppManifestStartResponse, GithubAppSettingsResponse, GithubInstallationsResponse,
    GithubRepositoriesResponse, GithubRepositorySummary, RelayAck, RelayAckStatus,
    RelayClientHello, RelayEnvelope, RelayEvent, RelayGithubInstallationTokenRequest,
    RelayGithubInstallationTokenResponse, RelayGithubRepositoriesRequest,
    RelayGithubRepositoryGetRequest, RelayGithubRepositoryPackagesRequest, RelayRequest,
    RelayResponse, RelayStatusResponse, RelayUpdateActionResponse, RelayUpdateApplyRequest,
    RelayUpdateCheckRequest, RelayUpdateRestartRequest, RelayUpdateRollbackRequest,
    RelayUpdateStatusResponse, RepositoryPackagesResponse,
};
use mai_runtime::RuntimeError;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Duration, sleep};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::RelayClientConfig;
use crate::protocol;

const RELAY_CONNECT_WAIT_SECS: u64 = 10;
const RELAY_RPC_TIMEOUT_SECS: u64 = 30;

pub struct RelayClient {
    config: RelayClientConfig,
    state: Arc<Mutex<RelayClientState>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<RelayResponse>>>>,
    event_handler: Arc<Mutex<Option<RelayEventHandler>>>,
    cancellation: CancellationToken,
}

type RelayEventHandler = Arc<
    dyn Fn(RelayEvent) -> Pin<Box<dyn Future<Output = Result<RelayAckStatus, String>> + Send>>
        + Send
        + Sync,
>;

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
            state: Arc::new(Mutex::new(RelayClientState::default())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            event_handler: Arc::new(Mutex::new(None)),
            cancellation: CancellationToken::new(),
        }
    }

    pub async fn set_event_handler<F, Fut>(&self, handler: F)
    where
        F: Fn(RelayEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<RelayAckStatus, String>> + Send + 'static,
    {
        let handler = Arc::new(move |event| {
            Box::pin(handler(event))
                as Pin<Box<dyn Future<Output = Result<RelayAckStatus, String>> + Send>>
        });
        *self.event_handler.lock().await = Some(handler);
    }

    pub fn start(self: Arc<Self>) {
        tokio::spawn(async move {
            self.run().await;
        });
    }

    pub fn stop(&self) {
        self.cancellation.cancel();
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
        let sender = match self.wait_for_sender().await {
            Some(sender) => sender,
            None => {
                return Err(RuntimeError::InvalidInput(
                    "relay is not connected".to_string(),
                ));
            }
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
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
        let response = match tokio::time::timeout(Duration::from_secs(RELAY_RPC_TIMEOUT_SECS), rx)
            .await
        {
            Ok(response) => response
                .map_err(|_| RuntimeError::InvalidInput("relay connection closed".to_string()))?,
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(RuntimeError::InvalidInput(
                    "relay request timed out".to_string(),
                ));
            }
        };
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

    async fn wait_for_sender(&self) -> Option<mpsc::UnboundedSender<RelayEnvelope>> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(RELAY_CONNECT_WAIT_SECS);
        loop {
            if let Some(sender) = {
                let state = self.state.lock().await;
                state.sender.clone()
            } {
                return Some(sender);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return None;
            }
            let sleep_for = (deadline - now).min(Duration::from_millis(50));
            tokio::select! {
                _ = self.cancellation.cancelled() => return None,
                _ = sleep(sleep_for) => {}
            }
        }
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

    pub async fn refresh_github_app_settings(
        &self,
    ) -> Result<GithubAppSettingsResponse, RuntimeError> {
        self.request("github.app.refresh", json!({})).await
    }

    pub async fn save_github_app_settings(
        &self,
        request: mai_protocol::GithubAppSettingsRequest,
    ) -> Result<GithubAppSettingsResponse, RuntimeError> {
        self.request("github.app.save", request).await
    }

    pub async fn relay_config(&self) -> Result<mai_protocol::RelaySettingsResponse, RuntimeError> {
        self.request("relay.config.get", json!({})).await
    }

    pub async fn save_relay_config(
        &self,
        request: mai_protocol::RelaySettingsRequest,
    ) -> Result<mai_protocol::RelaySettingsResponse, RuntimeError> {
        self.request("relay.config.save", request).await
    }

    pub async fn check_relay_update(
        &self,
        request: RelayUpdateCheckRequest,
    ) -> Result<RelayUpdateStatusResponse, RuntimeError> {
        self.request("relay.update.check", request).await
    }

    pub async fn apply_relay_update(&self) -> Result<RelayUpdateActionResponse, RuntimeError> {
        self.request("relay.update.apply", RelayUpdateApplyRequest {})
            .await
    }

    pub async fn rollback_relay_update(&self) -> Result<RelayUpdateActionResponse, RuntimeError> {
        self.request("relay.update.rollback", RelayUpdateRollbackRequest {})
            .await
    }

    pub async fn restart_relay(&self) -> Result<RelayUpdateActionResponse, RuntimeError> {
        self.request("relay.update.restart", RelayUpdateRestartRequest {})
            .await
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
            let result = tokio::select! {
                _ = self.cancellation.cancelled() => break,
                result = self.connect_once() => result,
            };
            match result {
                Ok(()) => {
                    delay = Duration::from_secs(1);
                }
                Err(err) => {
                    warn!("relay connection failed: {err}");
                    self.mark_disconnected(Some(err.to_string())).await;
                }
            }
            tokio::select! {
                _ = self.cancellation.cancelled() => break,
                _ = sleep(delay) => {}
            }
            delay = (delay * 2).min(Duration::from_secs(60));
        }
        self.mark_disconnected(Some("relay connection stopped".to_string()))
            .await;
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
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<RelayEvent>();
        let event_ack_tx = tx.clone();
        let event_handler = Arc::clone(&self.event_handler);
        let event_task = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let delivery_id = event.delivery_id.clone();
                let handler = event_handler.lock().await.clone();
                let ack = match handler {
                    Some(handler) => match handler(event).await {
                        Ok(status) => RelayAck {
                            delivery_id,
                            status,
                            message: None,
                        },
                        Err(message) => RelayAck {
                            delivery_id,
                            status: RelayAckStatus::Failed,
                            message: Some(message),
                        },
                    },
                    None => RelayAck {
                        delivery_id,
                        status: RelayAckStatus::Ignored,
                        message: Some("relay event receiver is not running".to_string()),
                    },
                };
                let _ = event_ack_tx.send(RelayEnvelope::Ack(ack));
            }
        });

        let read_result: Result<(), RuntimeError> = loop {
            let Some(message) = (tokio::select! {
                _ = self.cancellation.cancelled() => break Ok(()),
                message = reader.next() => message,
            }) else {
                break Ok(());
            };
            let message = match message {
                Ok(message) => message,
                Err(err) => {
                    break Err(RuntimeError::InvalidInput(format!(
                        "relay read failed: {err}"
                    )));
                }
            };
            let text = match message {
                Message::Text(text) => text.to_string(),
                Message::Ping(payload) => {
                    let _ = tx.send(RelayEnvelope::Pong {
                        id: String::from_utf8_lossy(&payload).to_string(),
                    });
                    continue;
                }
                Message::Close(_) => break Ok(()),
                Message::Binary(_) | Message::Frame(_) | Message::Pong(_) => continue,
            };
            let envelope = match serde_json::from_str::<RelayEnvelope>(&text) {
                Ok(envelope) => envelope,
                Err(err) => {
                    break Err(RuntimeError::InvalidInput(format!(
                        "invalid relay envelope: {err}"
                    )));
                }
            };
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
                    let _ = event_tx.send(event);
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
        };
        write_task.abort();
        event_task.abort();
        self.mark_disconnected(None).await;
        read_result
    }

    async fn mark_disconnected(&self, message: Option<String>) {
        {
            let mut state = self.state.lock().await;
            state.sender = None;
            state.connected = false;
            state.message = message;
        }
        self.pending.lock().await.clear();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{RelayEnvelope, RelayEvent, RelayEventKind};
    use pretty_assertions::assert_eq;
    use std::sync::Mutex as StdMutex;
    use tokio::net::TcpListener;
    use tokio::sync::{Mutex as TokioMutex, oneshot as test_oneshot};
    use tokio::time::{Duration, sleep, timeout};
    use tokio_tungstenite::accept_async;

    struct DropSignal(Option<test_oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    #[tokio::test]
    async fn request_waits_for_connection_started_in_background() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
        let addr = listener.local_addr().expect("relay addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept relay");
            let mut websocket = accept_async(stream).await.expect("websocket");
            let hello = websocket
                .next()
                .await
                .expect("hello")
                .expect("hello message");
            let RelayEnvelope::Hello(_) =
                serde_json::from_str::<RelayEnvelope>(&hello.into_text().expect("hello text"))
                    .expect("decode hello")
            else {
                panic!("first relay message should be hello");
            };
            let request = websocket
                .next()
                .await
                .expect("request")
                .expect("request message");
            let RelayEnvelope::Request(request) =
                serde_json::from_str::<RelayEnvelope>(&request.into_text().expect("request text"))
                    .expect("decode request")
            else {
                panic!("second relay message should be request");
            };
            assert_eq!(request.method, "github.installation_token.create");
            websocket
                .send(Message::Text(
                    serde_json::to_string(&RelayEnvelope::Response(RelayResponse {
                        id: request.id,
                        result: Some(json!({
                            "token": "relay-token",
                            "expires_at": "2026-05-18T00:00:00Z"
                        })),
                        error: None,
                    }))
                    .expect("encode response")
                    .into(),
                ))
                .await
                .expect("send response");
        });
        let client = Arc::new(RelayClient::new(RelayClientConfig {
            url: format!("http://{addr}"),
            token: "secret".to_string(),
            node_id: "node-1".to_string(),
        }));

        Arc::clone(&client).start();
        let response = client
            .create_installation_token(42, None, false)
            .await
            .expect("relay token");

        assert_eq!(response.token, "relay-token");
        assert_eq!(
            response.expires_at,
            "2026-05-18T00:00:00Z"
                .parse::<chrono::DateTime<chrono::Utc>>()
                .expect("expires")
        );
        server.await.expect("server");
    }

    #[tokio::test]
    async fn request_receives_response_while_event_handler_is_still_running() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
        let addr = listener.local_addr().expect("relay addr");
        let (handler_started_tx, handler_started_rx) = test_oneshot::channel();
        let handler_started = Arc::new(TokioMutex::new(Some(handler_started_tx)));
        let (release_handler_tx, release_handler_rx) = test_oneshot::channel();
        let release_handler = Arc::new(TokioMutex::new(Some(release_handler_rx)));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept relay");
            let mut websocket = accept_async(stream).await.expect("websocket");
            let hello = websocket
                .next()
                .await
                .expect("hello")
                .expect("hello message");
            let RelayEnvelope::Hello(_) =
                serde_json::from_str::<RelayEnvelope>(&hello.into_text().expect("hello text"))
                    .expect("decode hello")
            else {
                panic!("first relay message should be hello");
            };

            websocket
                .send(Message::Text(
                    serde_json::to_string(&RelayEnvelope::Event(RelayEvent {
                        sequence: 1,
                        delivery_id: "delivery-1".to_string(),
                        kind: RelayEventKind::Push,
                        payload: json!({}),
                    }))
                    .expect("encode event")
                    .into(),
                ))
                .await
                .expect("send event");

            let request = websocket
                .next()
                .await
                .expect("request")
                .expect("request message");
            let RelayEnvelope::Request(request) =
                serde_json::from_str::<RelayEnvelope>(&request.into_text().expect("request text"))
                    .expect("decode request")
            else {
                panic!("second client message should be request");
            };

            websocket
                .send(Message::Text(
                    serde_json::to_string(&RelayEnvelope::Response(RelayResponse {
                        id: request.id,
                        result: Some(json!({
                            "token": "relay-token",
                            "expires_at": "2026-05-18T00:00:00Z"
                        })),
                        error: None,
                    }))
                    .expect("encode response")
                    .into(),
                ))
                .await
                .expect("send response");

            let ack = websocket.next().await.expect("ack").expect("ack message");
            let RelayEnvelope::Ack(ack) =
                serde_json::from_str::<RelayEnvelope>(&ack.into_text().expect("ack text"))
                    .expect("decode ack")
            else {
                panic!("client should ack relay event");
            };
            assert_eq!(ack.delivery_id, "delivery-1");
            assert_eq!(ack.status, RelayAckStatus::Processed);
        });
        let client = Arc::new(RelayClient::new(RelayClientConfig {
            url: format!("http://{addr}"),
            token: "secret".to_string(),
            node_id: "node-1".to_string(),
        }));
        client
            .set_event_handler({
                let handler_started = Arc::clone(&handler_started);
                let release_handler = Arc::clone(&release_handler);
                move |_event| {
                    let handler_started = Arc::clone(&handler_started);
                    let release_handler = Arc::clone(&release_handler);
                    async move {
                        if let Some(tx) = handler_started.lock().await.take() {
                            let _ = tx.send(());
                        }
                        if let Some(rx) = release_handler.lock().await.take() {
                            let _ = rx.await;
                        }
                        Ok(RelayAckStatus::Processed)
                    }
                }
            })
            .await;

        Arc::clone(&client).start();
        timeout(Duration::from_secs(5), handler_started_rx)
            .await
            .expect("handler should start")
            .expect("handler signal should send");

        let response = timeout(
            Duration::from_secs(5),
            client.create_installation_token(42, None, false),
        )
        .await
        .expect("request should not wait for event handler")
        .expect("relay token");

        assert_eq!(response.token, "relay-token");
        let _ = release_handler_tx.send(());
        server.await.expect("server");
    }

    #[tokio::test]
    async fn request_finishes_when_connection_closes_before_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
        let addr = listener.local_addr().expect("relay addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept relay");
            let mut websocket = accept_async(stream).await.expect("websocket");
            let hello = websocket
                .next()
                .await
                .expect("hello")
                .expect("hello message");
            let RelayEnvelope::Hello(_) =
                serde_json::from_str::<RelayEnvelope>(&hello.into_text().expect("hello text"))
                    .expect("decode hello")
            else {
                panic!("first relay message should be hello");
            };
            let request = websocket
                .next()
                .await
                .expect("request")
                .expect("request message");
            let RelayEnvelope::Request(_) =
                serde_json::from_str::<RelayEnvelope>(&request.into_text().expect("request text"))
                    .expect("decode request")
            else {
                panic!("second relay message should be request");
            };
            websocket.close(None).await.expect("close websocket");
        });
        let client = Arc::new(RelayClient::new(RelayClientConfig {
            url: format!("http://{addr}"),
            token: "secret".to_string(),
            node_id: "node-1".to_string(),
        }));

        Arc::clone(&client).start();
        let err = timeout(
            Duration::from_secs(5),
            client.create_installation_token(42, None, false),
        )
        .await
        .expect("request should finish when relay closes")
        .expect_err("relay request should fail");

        assert!(err.to_string().contains("relay connection closed"));
        server.await.expect("server");
    }

    #[tokio::test]
    async fn event_worker_stops_when_connection_exits_with_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
        let addr = listener.local_addr().expect("relay addr");
        let (handler_started_tx, handler_started_rx) = test_oneshot::channel();
        let handler_started = Arc::new(TokioMutex::new(Some(handler_started_tx)));
        let (handler_dropped_tx, handler_dropped_rx) = test_oneshot::channel();
        let handler_dropped = Arc::new(StdMutex::new(Some(handler_dropped_tx)));
        let (release_handler_tx, release_handler_rx) = test_oneshot::channel();
        let release_handler = Arc::new(TokioMutex::new(Some(release_handler_rx)));
        let (send_invalid_tx, send_invalid_rx) = test_oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept relay");
            let mut websocket = accept_async(stream).await.expect("websocket");
            let hello = websocket
                .next()
                .await
                .expect("hello")
                .expect("hello message");
            let RelayEnvelope::Hello(_) =
                serde_json::from_str::<RelayEnvelope>(&hello.into_text().expect("hello text"))
                    .expect("decode hello")
            else {
                panic!("first relay message should be hello");
            };

            websocket
                .send(Message::Text(
                    serde_json::to_string(&RelayEnvelope::Event(RelayEvent {
                        sequence: 1,
                        delivery_id: "delivery-1".to_string(),
                        kind: RelayEventKind::Push,
                        payload: json!({}),
                    }))
                    .expect("encode event")
                    .into(),
                ))
                .await
                .expect("send event");

            send_invalid_rx.await.expect("send invalid signal");
            websocket
                .send(Message::Text("not a relay envelope".into()))
                .await
                .expect("send invalid envelope");
        });
        let client = Arc::new(RelayClient::new(RelayClientConfig {
            url: format!("http://{addr}"),
            token: "secret".to_string(),
            node_id: "node-1".to_string(),
        }));
        client
            .set_event_handler({
                let handler_started = Arc::clone(&handler_started);
                let handler_dropped = Arc::clone(&handler_dropped);
                let release_handler = Arc::clone(&release_handler);
                move |_event| {
                    let handler_started = Arc::clone(&handler_started);
                    let handler_dropped = Arc::clone(&handler_dropped);
                    let release_handler = Arc::clone(&release_handler);
                    async move {
                        let _drop_signal = handler_dropped
                            .lock()
                            .expect("handler dropped mutex")
                            .take()
                            .map(|tx| DropSignal(Some(tx)));
                        if let Some(tx) = handler_started.lock().await.take() {
                            let _ = tx.send(());
                        }
                        if let Some(rx) = release_handler.lock().await.take() {
                            let _ = rx.await;
                        }
                        Ok(RelayAckStatus::Processed)
                    }
                }
            })
            .await;

        Arc::clone(&client).start();
        timeout(Duration::from_secs(5), handler_started_rx)
            .await
            .expect("handler should start")
            .expect("handler signal should send");
        send_invalid_tx.send(()).expect("send invalid signal");

        timeout(Duration::from_secs(5), handler_dropped_rx)
            .await
            .expect("handler should be dropped when connection exits")
            .expect("handler dropped signal should send");

        let _ = release_handler_tx.send(());
        client.stop();
        server.await.expect("server");
    }

    #[tokio::test]
    async fn request_started_before_reconnect_is_not_cleared_before_send() {
        let probe = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind probe relay");
        let addr = probe.local_addr().expect("probe relay addr");
        drop(probe);
        let client = Arc::new(RelayClient::new(RelayClientConfig {
            url: format!("http://{addr}"),
            token: "secret".to_string(),
            node_id: "node-1".to_string(),
        }));

        Arc::clone(&client).start();
        let request = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.create_installation_token(42, None, false).await }
        });
        sleep(Duration::from_millis(1200)).await;

        let listener = TcpListener::bind(addr).await.expect("bind relay");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept relay");
            let mut websocket = accept_async(stream).await.expect("websocket");
            let hello = websocket
                .next()
                .await
                .expect("hello")
                .expect("hello message");
            let RelayEnvelope::Hello(_) =
                serde_json::from_str::<RelayEnvelope>(&hello.into_text().expect("hello text"))
                    .expect("decode hello")
            else {
                panic!("first relay message should be hello");
            };
            let request = websocket
                .next()
                .await
                .expect("request")
                .expect("request message");
            let RelayEnvelope::Request(request) =
                serde_json::from_str::<RelayEnvelope>(&request.into_text().expect("request text"))
                    .expect("decode request")
            else {
                panic!("second relay message should be request");
            };
            websocket
                .send(Message::Text(
                    serde_json::to_string(&RelayEnvelope::Response(RelayResponse {
                        id: request.id,
                        result: Some(json!({
                            "token": "relay-token",
                            "expires_at": "2026-05-18T00:00:00Z"
                        })),
                        error: None,
                    }))
                    .expect("encode response")
                    .into(),
                ))
                .await
                .expect("send response");
        });

        let response = timeout(Duration::from_secs(8), request)
            .await
            .expect("request should finish after reconnect")
            .expect("request task should finish")
            .expect("relay token");

        assert_eq!(response.token, "relay-token");
        client.stop();
        server.await.expect("server");
    }
}
