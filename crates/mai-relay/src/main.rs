use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, TimeDelta, Utc};
use futures::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use mai_protocol::{
    GithubAppInstallationStartRequest, GithubAppInstallationStartResponse,
    GithubAppManifestAccountType, GithubAppManifestStartRequest, GithubAppManifestStartResponse,
    GithubAppSettingsResponse, GithubInstallationSummary, GithubInstallationsResponse,
    GithubRepositoriesResponse, GithubRepositorySummary, RelayAck, RelayAckStatus, RelayEnvelope,
    RelayError, RelayEvent, RelayEventKind, RelayGithubInstallationTokenRequest,
    RelayGithubInstallationTokenResponse, RelayGithubRepositoriesRequest,
    RelayGithubRepositoryPackagesRequest, RelayRequest, RelayResponse, RelayStatusResponse,
    RepositoryPackageSummary, RepositoryPackagesResponse,
};
use reqwest::header::{ACCEPT, HeaderValue, USER_AGENT};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::Duration;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use uuid::Uuid;

const GITHUB_API_VERSION: &str = "2022-11-28";
const DEFAULT_GITHUB_API_BASE_URL: &str = "https://api.github.com";
const DEFAULT_GITHUB_WEB_BASE_URL: &str = "https://github.com";
const TOKEN_REFRESH_SKEW_SECS: i64 = 120;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
struct AppState {
    store: Arc<RelayStore>,
    token: String,
    public_url: String,
    github_api_base_url: String,
    github_web_base_url: String,
    http: reqwest::Client,
    connection: Arc<Mutex<Option<ActiveConnection>>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<RelayResponse>>>>,
    sequence: Arc<AtomicU64>,
}

#[derive(Clone)]
struct ActiveConnection {
    node_id: String,
    sender: mpsc::UnboundedSender<RelayEnvelope>,
    last_heartbeat_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct GithubJwtClaims {
    iat: usize,
    exp: usize,
    iss: String,
}

#[derive(Debug, Deserialize)]
struct GithubManifestCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubInstallationCallbackQuery {
    setup_action: Option<String>,
    installation_id: Option<u64>,
    state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GithubAppConfig {
    app_id: String,
    private_key: String,
    #[serde(default)]
    webhook_secret: String,
    app_slug: Option<String>,
    app_html_url: Option<String>,
    owner_login: Option<String>,
    owner_type: Option<String>,
}

#[derive(Debug, Clone)]
struct ManifestState {
    state: String,
    created_at: DateTime<Utc>,
    account_type: GithubAppManifestAccountType,
    org: Option<String>,
}

#[derive(Debug, Clone)]
struct InstallationState {
    state: String,
    created_at: DateTime<Utc>,
    origin: String,
    return_hash: String,
}

#[derive(Debug, Clone)]
struct QueuedDelivery {
    sequence: u64,
    delivery_id: String,
    event_name: String,
    payload: Value,
}

#[derive(Debug, Deserialize)]
struct GithubAccountApi {
    login: String,
    #[serde(rename = "type")]
    account_type: String,
}

#[derive(Debug, Deserialize)]
struct GithubInstallationApi {
    id: u64,
    account: GithubAccountApi,
    repository_selection: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubRepositoriesApi {
    repositories: Vec<GithubRepositoryApi>,
}

#[derive(Debug, Deserialize)]
struct GithubRepositoryApi {
    id: u64,
    name: String,
    full_name: String,
    private: bool,
    clone_url: String,
    html_url: String,
    default_branch: Option<String>,
    owner: GithubAccountApi,
}

#[derive(Debug, Deserialize)]
struct GithubPackageApi {
    name: String,
    html_url: String,
    #[serde(default)]
    repository: Option<GithubPackageRepositoryApi>,
}

#[derive(Debug, Deserialize)]
struct GithubPackageRepositoryApi {
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct GithubPackageVersionApi {
    #[serde(default)]
    metadata: GithubPackageVersionMetadataApi,
}

#[derive(Debug, Deserialize, Default)]
struct GithubPackageVersionMetadataApi {
    #[serde(default)]
    container: GithubPackageContainerMetadataApi,
}

#[derive(Debug, Deserialize, Default)]
struct GithubPackageContainerMetadataApi {
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Serialize)]
struct GithubAccessTokenRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_ids: Option<Vec<u64>>,
    permissions: GithubAccessTokenPermissions,
}

#[derive(Debug, Serialize)]
struct GithubAccessTokenPermissions {
    contents: &'static str,
    pull_requests: &'static str,
    issues: &'static str,
}

#[derive(Debug, Deserialize)]
struct GithubAccessTokenResponse {
    token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct GithubManifestConversionResponse {
    id: u64,
    slug: String,
    html_url: String,
    pem: String,
    #[serde(default)]
    owner: Option<GithubAccountApi>,
    #[serde(default)]
    webhook_secret: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubAppApi {
    slug: String,
    html_url: String,
    #[serde(default)]
    owner: Option<GithubAccountApi>,
}

#[derive(Debug, Deserialize)]
struct GithubErrorResponse {
    message: Option<String>,
}

#[derive(Debug, Serialize)]
struct GithubAppHookConfigRequest {
    url: String,
    content_type: &'static str,
    insecure_ssl: &'static str,
    secret: String,
}

#[derive(Debug, Deserialize)]
struct GithubAppHookConfigResponse {
    url: Option<String>,
}

#[derive(Debug)]
enum GithubHookReset {
    Updated,
    Missing,
}

#[derive(Debug, thiserror::Error)]
enum RelayErrorKind {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("jwt error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

impl IntoResponse for RelayErrorKind {
    fn into_response(self) -> Response {
        let status = match self {
            RelayErrorKind::InvalidInput(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

type RelayResult<T> = std::result::Result<T, RelayErrorKind>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mai_relay=info,tower_http=info".into()),
        )
        .init();

    let bind = env::var("MAI_RELAY_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8090".to_string());
    let addr: SocketAddr = bind.parse().context("invalid MAI_RELAY_BIND_ADDR")?;
    let public_url = env::var("MAI_RELAY_PUBLIC_URL")
        .unwrap_or_else(|_| format!("http://127.0.0.1:{}", addr.port()))
        .trim_end_matches('/')
        .to_string();
    let token = env::var("MAI_RELAY_TOKEN").context("MAI_RELAY_TOKEN is required")?;
    let db_path = env::var("MAI_RELAY_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("mai-relay.sqlite3"));
    let github_api_base_url = env::var("GITHUB_API_BASE_URL")
        .unwrap_or_else(|_| DEFAULT_GITHUB_API_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string();
    let github_web_base_url = env::var("GITHUB_WEB_BASE_URL")
        .unwrap_or_else(|_| DEFAULT_GITHUB_WEB_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string();

    let store = Arc::new(RelayStore::open(db_path)?);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    bootstrap_github_app_config(
        &store,
        &http,
        &public_url,
        &github_api_base_url,
        &github_web_base_url,
    )
    .await?;
    let sequence = Arc::new(AtomicU64::new(store.next_sequence()?));
    let state = AppState {
        store,
        token,
        public_url,
        github_api_base_url,
        github_web_base_url,
        http,
        connection: Arc::new(Mutex::new(None)),
        pending: Arc::new(Mutex::new(HashMap::new())),
        sequence,
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/relay/v1/connect", get(connect))
        .route("/relay/v1/status", get(status))
        .route("/github/app-manifest/callback", get(app_manifest_callback))
        .route(
            "/github/app-installation/callback",
            get(app_installation_callback),
        )
        .route("/github/webhook", post(webhook))
        .layer(TraceLayer::new_for_http())
        .with_state(Arc::new(state));

    info!("mai-relay listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

async fn status(State(state): State<Arc<AppState>>) -> RelayResult<Json<RelayStatusResponse>> {
    let connection = state.connection.lock().await.clone();
    let queued = state.store.queued_count()?;
    Ok(Json(RelayStatusResponse {
        enabled: true,
        connected: connection.is_some(),
        relay_url: Some(state.public_url.clone()),
        node_id: connection
            .as_ref()
            .map(|connection| connection.node_id.clone()),
        last_heartbeat_at: connection.map(|connection| connection.last_heartbeat_at),
        queued_deliveries: Some(queued),
        message: None,
    }))
}

async fn connect(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state, socket))
}

async fn handle_socket(state: Arc<AppState>, socket: WebSocket) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let Some(Ok(Message::Text(text))) = ws_receiver.next().await else {
        return;
    };
    let Ok(RelayEnvelope::Hello(hello)) = serde_json::from_str::<RelayEnvelope>(&text) else {
        let _ = ws_sender
            .send(Message::Text(
                serde_json::to_string(&RelayEnvelope::Response(RelayResponse {
                    id: "hello".to_string(),
                    result: None,
                    error: Some(RelayError {
                        code: "invalid_hello".to_string(),
                        message: "first message must be hello".to_string(),
                    }),
                }))
                .unwrap()
                .into(),
            ))
            .await;
        return;
    };
    if hello.token != state.token {
        let _ = ws_sender.close().await;
        return;
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<RelayEnvelope>();
    {
        let mut connection = state.connection.lock().await;
        *connection = Some(ActiveConnection {
            node_id: hello.node_id.clone(),
            sender: tx.clone(),
            last_heartbeat_at: Utc::now(),
        });
    }
    info!(node_id = %hello.node_id, "relay client connected");
    if let Err(err) = replay_queued(&state, &tx).await {
        warn!("failed to replay queued deliveries: {err}");
    }

    let write_task = tokio::spawn(async move {
        while let Some(envelope) = rx.recv().await {
            match serde_json::to_string(&envelope) {
                Ok(text) => {
                    if ws_sender.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(err) => warn!("failed to serialize relay envelope: {err}"),
            }
        }
    });

    while let Some(message) = ws_receiver.next().await {
        let message = match message {
            Ok(Message::Text(text)) => text.to_string(),
            Ok(Message::Pong(_)) => {
                touch_connection(&state, &hello.node_id).await;
                continue;
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };
        let Ok(envelope) = serde_json::from_str::<RelayEnvelope>(&message) else {
            warn!("received invalid relay envelope from client");
            continue;
        };
        match envelope {
            RelayEnvelope::Response(response) => {
                if let Some(tx) = state.pending.lock().await.remove(&response.id) {
                    let _ = tx.send(response);
                }
            }
            RelayEnvelope::Ack(ack) => {
                if let Err(err) = handle_ack(&state, ack).await {
                    warn!("failed to handle ack: {err}");
                }
            }
            RelayEnvelope::Pong { .. } => {
                touch_connection(&state, &hello.node_id).await;
            }
            RelayEnvelope::Request(request) => {
                let state = Arc::clone(&state);
                let tx = tx.clone();
                tokio::spawn(async move {
                    let response = handle_client_request(&state, request).await;
                    let _ = tx.send(RelayEnvelope::Response(response));
                });
            }
            _ => {}
        }
    }

    write_task.abort();
    {
        let mut connection = state.connection.lock().await;
        if connection
            .as_ref()
            .is_some_and(|connection| connection.node_id == hello.node_id)
        {
            *connection = None;
        }
    }
    info!(node_id = %hello.node_id, "relay client disconnected");
}

async fn replay_queued(
    state: &Arc<AppState>,
    tx: &mpsc::UnboundedSender<RelayEnvelope>,
) -> RelayResult<()> {
    for delivery in state.store.list_unacked_deliveries()? {
        tx.send(RelayEnvelope::Event(delivery.into_event())).ok();
    }
    Ok(())
}

async fn touch_connection(state: &Arc<AppState>, node_id: &str) {
    let mut connection = state.connection.lock().await;
    if let Some(connection) = connection.as_mut()
        && connection.node_id == node_id
    {
        connection.last_heartbeat_at = Utc::now();
    }
}

async fn handle_ack(state: &Arc<AppState>, ack: RelayAck) -> RelayResult<()> {
    match ack.status {
        RelayAckStatus::Processed | RelayAckStatus::Ignored => {
            state.store.ack_delivery(&ack.delivery_id)?;
        }
        RelayAckStatus::Failed => {
            warn!(
                delivery_id = %ack.delivery_id,
                message = ack.message.as_deref().unwrap_or(""),
                "relay client failed delivery"
            );
        }
    }
    Ok(())
}

async fn handle_client_request(state: &Arc<AppState>, request: RelayRequest) -> RelayResponse {
    let id = request.id.clone();
    let result = match request.method.as_str() {
        "github_app_manifest.start" => {
            match parse_params::<GithubAppManifestStartRequest>(request.params).await {
                Ok(request) => start_manifest(state, request).await,
                Err(err) => Err(err),
            }
        }
        "github.app.get" => github_app_settings(state).and_then(to_value),
        "github.app_installation.start" => {
            match parse_params::<GithubAppInstallationStartRequest>(request.params).await {
                Ok(request) => start_app_installation(state, request)
                    .await
                    .and_then(to_value),
                Err(err) => Err(err),
            }
        }
        "github.installations.list" => list_installations(state).await.and_then(to_value),
        "github.repositories.list" => {
            match parse_params::<RelayGithubRepositoriesRequest>(request.params).await {
                Ok(request) => list_repositories(state, request.installation_id).await,
                Err(err) => Err(err),
            }
        }
        "github.repository.get" => {
            match parse_params::<mai_protocol::RelayGithubRepositoryGetRequest>(request.params)
                .await
            {
                Ok(request) => get_repository(state, request).await,
                Err(err) => Err(err),
            }
        }
        "github.installation_token.create" => {
            match parse_params::<RelayGithubInstallationTokenRequest>(request.params).await {
                Ok(request) => create_installation_token(state, request)
                    .await
                    .and_then(to_value),
                Err(err) => Err(err),
            }
        }
        "github.repository_packages.list" => {
            match parse_params::<RelayGithubRepositoryPackagesRequest>(request.params).await {
                Ok(request) => list_repository_packages(state, request)
                    .await
                    .and_then(to_value),
                Err(err) => Err(err),
            }
        }
        "github.webhook_delivery.ack" => match parse_params::<RelayAck>(request.params).await {
            Ok(ack) => match handle_ack(state, ack).await {
                Ok(()) => Ok(json!({ "ok": true })),
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        },
        other => Err(RelayErrorKind::InvalidInput(format!(
            "unknown relay method `{other}`"
        ))),
    };
    relay_response(id, result)
}

async fn app_manifest_callback(
    State(state): State<Arc<AppState>>,
    Query(query): Query<GithubManifestCallbackQuery>,
) -> Response {
    if let Some(error) = query.error {
        let message = query.error_description.unwrap_or(error);
        return callback_page(false, "GitHub App setup was cancelled", &message);
    }
    let code = query.code.unwrap_or_default();
    let state_value = query.state.unwrap_or_default();
    match complete_manifest(&state, &code, &state_value).await {
        Ok(_) => callback_page(
            true,
            "GitHub App connected",
            "Mai Relay saved the GitHub App.",
        ),
        Err(err) => callback_page(false, "GitHub App setup failed", &err.to_string()),
    }
}

async fn app_installation_callback(
    State(state): State<Arc<AppState>>,
    Query(query): Query<GithubInstallationCallbackQuery>,
) -> Response {
    match complete_app_installation(&state, &query).await {
        Ok(response) => response,
        Err(err) => callback_page(
            false,
            "GitHub App installation could not be completed",
            &err.to_string(),
        ),
    }
}

async fn webhook(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let delivery_id = header_string(&headers, "x-github-delivery").unwrap_or_default();
    let event_name = header_string(&headers, "x-github-event").unwrap_or_default();
    if delivery_id.trim().is_empty() || event_name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "missing GitHub webhook headers").into_response();
    }
    let config = match state.store.github_app_config() {
        Ok(Some(config)) => config,
        Ok(None) => {
            return (StatusCode::BAD_REQUEST, "GitHub App is not configured").into_response();
        }
        Err(err) => return err.into_response(),
    };
    let signature = header_string(&headers, "x-hub-signature-256").unwrap_or_default();
    if !verify_signature(&config.webhook_secret, &body, &signature) {
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON payload").into_response(),
    };
    let sequence = state.sequence.fetch_add(1, Ordering::SeqCst);
    match state
        .store
        .insert_delivery(sequence, &delivery_id, &event_name, &payload)
    {
        Ok(inserted) => {
            if inserted {
                let delivery = QueuedDelivery {
                    sequence,
                    delivery_id: delivery_id.clone(),
                    event_name: event_name.clone(),
                    payload,
                };
                if let Some(connection) = state.connection.lock().await.clone() {
                    let _ = connection
                        .sender
                        .send(RelayEnvelope::Event(delivery.into_event()));
                }
            }
            (StatusCode::ACCEPTED, "accepted").into_response()
        }
        Err(err) => err.into_response(),
    }
}

async fn start_manifest(
    state: &AppState,
    request: GithubAppManifestStartRequest,
) -> RelayResult<Value> {
    let origin = if request.origin.trim().is_empty() {
        state.public_url.clone()
    } else {
        sanitize_origin(&request.origin)?
    };
    let org = match request.account_type {
        GithubAppManifestAccountType::Organization => Some(
            request
                .org
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    RelayErrorKind::InvalidInput("organization is required".to_string())
                })?
                .to_string(),
        ),
        GithubAppManifestAccountType::Personal => None,
    };
    if let Some(org) = &org
        && !is_valid_github_slug(org)
    {
        return Err(RelayErrorKind::InvalidInput(
            "organization may contain only letters, numbers, or hyphens".to_string(),
        ));
    }
    let state_id = Uuid::new_v4().to_string();
    let redirect_url = format!("{origin}/github/app-manifest/callback");
    let setup_url = format!("{origin}/github/app-installation/callback");
    let webhook_url = format!("{origin}/github/webhook");
    let webhook_secret = Uuid::new_v4().to_string();
    let manifest = github_app_manifest(&redirect_url, &setup_url, &webhook_url, &webhook_secret);
    let action_url = match (&request.account_type, &org) {
        (GithubAppManifestAccountType::Organization, Some(org)) => {
            format!(
                "{}/organizations/{}/settings/apps/new?state={}",
                state.github_web_base_url, org, state_id
            )
        }
        _ => format!(
            "{}/settings/apps/new?state={state_id}",
            state.github_web_base_url
        ),
    };
    state.store.save_manifest_state(
        &ManifestState {
            state: state_id.clone(),
            created_at: Utc::now(),
            account_type: request.account_type,
            org,
        },
        &webhook_secret,
    )?;
    to_value(GithubAppManifestStartResponse {
        state: state_id,
        action_url,
        manifest,
    })
}

async fn complete_manifest(state: &AppState, code: &str, state_id: &str) -> RelayResult<()> {
    if !is_valid_manifest_code(code) {
        return Err(RelayErrorKind::InvalidInput(
            "invalid GitHub manifest code".to_string(),
        ));
    }
    let (manifest_state, saved_webhook_secret) = state.store.take_manifest_state(state_id)?;
    let url = github_api_url(
        &state.github_api_base_url,
        &format!("/app-manifests/{code}/conversions"),
    );
    let response = state
        .http
        .post(url)
        .headers(github_headers())
        .send()
        .await?;
    let conversion: GithubManifestConversionResponse =
        decode_github_response(response, "create app from manifest").await?;
    let owner_login = conversion
        .owner
        .as_ref()
        .map(|owner| owner.login.clone())
        .or_else(|| {
            manifest_state.org.clone().filter(|_| {
                manifest_state.account_type == GithubAppManifestAccountType::Organization
            })
        });
    let owner_type = conversion
        .owner
        .as_ref()
        .map(|owner| owner.account_type.clone())
        .or_else(|| match manifest_state.account_type {
            GithubAppManifestAccountType::Organization => Some("Organization".to_string()),
            GithubAppManifestAccountType::Personal => Some("User".to_string()),
        });
    state.store.save_github_app_config(&GithubAppConfig {
        app_id: conversion.id.to_string(),
        private_key: conversion.pem,
        webhook_secret: conversion.webhook_secret.unwrap_or(saved_webhook_secret),
        app_slug: Some(conversion.slug),
        app_html_url: Some(conversion.html_url),
        owner_login,
        owner_type,
    })?;
    Ok(())
}

fn github_app_settings(state: &AppState) -> RelayResult<GithubAppSettingsResponse> {
    let config = state
        .store
        .github_app_config()?
        .ok_or_else(|| RelayErrorKind::InvalidInput("GitHub App is not configured".to_string()))?;
    Ok(github_app_settings_response(
        &config,
        &state.github_api_base_url,
        &state.github_web_base_url,
        None,
    ))
}

async fn start_app_installation(
    state: &AppState,
    request: GithubAppInstallationStartRequest,
) -> RelayResult<GithubAppInstallationStartResponse> {
    let config = state
        .store
        .github_app_config()?
        .ok_or_else(|| RelayErrorKind::InvalidInput("GitHub App is not configured".to_string()))?;
    let app_slug = config.app_slug.as_deref().ok_or_else(|| {
        RelayErrorKind::InvalidInput("GitHub App slug is required for installation".to_string())
    })?;
    let origin = if request.origin.trim().is_empty() {
        state.public_url.clone()
    } else {
        sanitize_origin(&request.origin)?
    };
    let return_hash = request
        .return_hash
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("#projects")
        .to_string();
    let state_id = Uuid::new_v4().to_string();
    state.store.save_installation_state(&InstallationState {
        state: state_id.clone(),
        created_at: Utc::now(),
        origin,
        return_hash,
    })?;
    let install_url = github_app_install_url(&state.github_web_base_url, app_slug, Some(&state_id));
    Ok(GithubAppInstallationStartResponse {
        state: state_id.clone(),
        install_url,
        app: github_app_settings_response(
            &config,
            &state.github_api_base_url,
            &state.github_web_base_url,
            Some(&state_id),
        ),
    })
}

async fn complete_app_installation(
    state: &AppState,
    query: &GithubInstallationCallbackQuery,
) -> RelayResult<Response> {
    let install_state = if let Some(state_id) = query
        .state
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        state.store.take_installation_state(state_id)?
    } else if query.installation_id.is_some() {
        state.store.take_latest_installation_state()?
    } else {
        return Ok(github_installation_fallback_page(query));
    };
    let params = match query.installation_id {
        Some(installation_id) => {
            verify_installation(state, installation_id).await?;
            format!("github-app=installed&installation_id={installation_id}")
        }
        None => "github-app=pending".to_string(),
    };
    let location = local_return_url(&install_state.origin, &install_state.return_hash, &params)?;
    Ok((
        StatusCode::FOUND,
        [(axum::http::header::LOCATION, location)],
        "",
    )
        .into_response())
}

fn github_installation_fallback_page(query: &GithubInstallationCallbackQuery) -> Response {
    let message = match (query.setup_action.as_deref(), query.installation_id) {
        (Some(action), Some(id)) => format!("GitHub App installation {action}: {id}"),
        (Some(action), None) => format!("GitHub App installation {action}."),
        (None, Some(id)) => format!("GitHub App installation ready: {id}"),
        (None, None) => "GitHub App installation finished.".to_string(),
    };
    callback_page(true, "GitHub App installation updated", &message)
}

async fn list_installations(state: &AppState) -> RelayResult<GithubInstallationsResponse> {
    let (jwt, base_url) = github_app_jwt(state)?;
    let url = github_api_url(&base_url, "/app/installations?per_page=100");
    let response = state
        .http
        .get(url)
        .bearer_auth(jwt)
        .headers(github_headers())
        .send()
        .await?;
    let installations: Vec<GithubInstallationApi> =
        decode_github_response(response, "list installations").await?;
    Ok(GithubInstallationsResponse {
        installations: installations
            .into_iter()
            .map(|installation| GithubInstallationSummary {
                id: installation.id,
                account_login: installation.account.login,
                account_type: installation.account.account_type,
                repository_selection: installation.repository_selection,
            })
            .collect(),
    })
}

async fn list_repositories(state: &AppState, installation_id: u64) -> RelayResult<Value> {
    let token = create_installation_token(
        state,
        RelayGithubInstallationTokenRequest {
            installation_id,
            repository_id: None,
        },
    )
    .await?;
    let url = github_api_url(
        &state.github_api_base_url,
        "/installation/repositories?per_page=100",
    );
    let response = state
        .http
        .get(url)
        .bearer_auth(token.token)
        .headers(github_headers())
        .send()
        .await?;
    let response: GithubRepositoriesApi =
        decode_github_response(response, "list installation repositories").await?;
    to_value(GithubRepositoriesResponse {
        repositories: response
            .repositories
            .into_iter()
            .map(github_repository_summary)
            .collect(),
    })
}

async fn verify_installation(state: &AppState, installation_id: u64) -> RelayResult<()> {
    let installations = list_installations(state).await?;
    if installations
        .installations
        .iter()
        .any(|installation| installation.id == installation_id)
    {
        Ok(())
    } else {
        Err(RelayErrorKind::InvalidInput(format!(
            "GitHub App installation {installation_id} was not found"
        )))
    }
}

async fn get_repository(
    state: &AppState,
    request: mai_protocol::RelayGithubRepositoryGetRequest,
) -> RelayResult<Value> {
    let token = create_installation_token(
        state,
        RelayGithubInstallationTokenRequest {
            installation_id: request.installation_id,
            repository_id: None,
        },
    )
    .await?;
    let path = format!("/repos/{}", request.repository_full_name);
    let url = github_api_url(&state.github_api_base_url, &path);
    let response = state
        .http
        .get(url)
        .bearer_auth(token.token)
        .headers(github_headers())
        .send()
        .await?;
    let repository: GithubRepositoryApi =
        decode_github_response(response, "get repository").await?;
    to_value(github_repository_summary(repository))
}

async fn list_repository_packages(
    state: &AppState,
    request: RelayGithubRepositoryPackagesRequest,
) -> RelayResult<RepositoryPackagesResponse> {
    if request.installation_id == 0 {
        return Err(RelayErrorKind::InvalidInput(
            "installation_id is required".to_string(),
        ));
    }
    let owner = request.owner.trim();
    let repo = request.repo.trim();
    if owner.is_empty() || repo.is_empty() {
        return Err(RelayErrorKind::InvalidInput(
            "repository owner and name are required".to_string(),
        ));
    }
    let token = create_installation_token(
        state,
        RelayGithubInstallationTokenRequest {
            installation_id: request.installation_id,
            repository_id: None,
        },
    )
    .await?;
    let repository_ref = format!("{owner}/{repo}");
    let packages = match github_container_packages_for_owner(state, &token.token, owner).await {
        Ok(packages) => packages,
        Err(err) if github_packages_read_error(err.status()) => {
            return Ok(RepositoryPackagesResponse {
                packages: Vec::new(),
                warning: Some("No readable GitHub container packages found for this owner".to_string()),
            });
        }
        Err(err) => return Err(RelayErrorKind::Http(err)),
    };
    let mut summaries = Vec::new();
    for package in packages
        .into_iter()
        .filter(|package| github_package_belongs_to_repo(package, &repository_ref))
    {
        let versions = match github_container_package_versions(
            state,
            &token.token,
            owner,
            &package.name,
        )
        .await
        {
            Ok(versions) => versions,
            Err(err) if github_packages_read_error(err.status()) => continue,
            Err(err) => return Err(RelayErrorKind::Http(err)),
        };
        if let Some(summary) = repository_package_summary(owner, package, versions) {
            summaries.push(summary);
        }
    }
    summaries.sort_by(|left, right| left.name.cmp(&right.name).then(left.tag.cmp(&right.tag)));
    Ok(RepositoryPackagesResponse {
        packages: summaries,
        warning: None,
    })
}

async fn create_installation_token(
    state: &AppState,
    request: RelayGithubInstallationTokenRequest,
) -> RelayResult<RelayGithubInstallationTokenResponse> {
    if request.installation_id == 0 {
        return Err(RelayErrorKind::InvalidInput(
            "installation_id is required".to_string(),
        ));
    }
    if let Some(cached) = state
        .store
        .cached_token(request.installation_id, request.repository_id)?
        && cached.expires_at - TimeDelta::seconds(TOKEN_REFRESH_SKEW_SECS) > Utc::now()
    {
        return Ok(cached);
    }
    let (jwt, base_url) = github_app_jwt(state)?;
    let url = github_api_url(
        &base_url,
        &format!(
            "/app/installations/{}/access_tokens",
            request.installation_id
        ),
    );
    let body = GithubAccessTokenRequest {
        repository_ids: request.repository_id.map(|id| vec![id]),
        permissions: GithubAccessTokenPermissions {
            contents: "write",
            pull_requests: "write",
            issues: "write",
        },
    };
    let response = state
        .http
        .post(url)
        .bearer_auth(jwt)
        .headers(github_headers())
        .json(&body)
        .send()
        .await?;
    let token: GithubAccessTokenResponse =
        decode_github_response(response, "create installation token").await?;
    let token = RelayGithubInstallationTokenResponse {
        token: token.token,
        expires_at: token.expires_at,
    };
    state
        .store
        .save_cached_token(request.installation_id, request.repository_id, &token)?;
    Ok(token)
}

fn github_app_jwt(state: &AppState) -> RelayResult<(String, String)> {
    let config = state
        .store
        .github_app_config()?
        .ok_or_else(|| RelayErrorKind::InvalidInput("GitHub App is not configured".to_string()))?;
    let token = github_app_jwt_for_config(&config)?;
    Ok((token, state.github_api_base_url.clone()))
}

fn github_app_jwt_for_config(config: &GithubAppConfig) -> RelayResult<String> {
    let now = Utc::now().timestamp();
    let claims = GithubJwtClaims {
        iat: now.saturating_sub(60) as usize,
        exp: now.saturating_add(540) as usize,
        iss: config.app_id.clone(),
    };
    let token = encode(
        &Header::new(Algorithm::RS256),
        &claims,
        &EncodingKey::from_rsa_pem(config.private_key.as_bytes())?,
    )?;
    Ok(token)
}

async fn github_container_packages_for_owner(
    state: &AppState,
    token: &str,
    owner: &str,
) -> std::result::Result<Vec<GithubPackageApi>, reqwest::Error> {
    let org_url = github_api_url(
        &state.github_api_base_url,
        &format!(
            "/orgs/{}/packages?package_type=container&per_page=100",
            github_path_segment(owner)
        ),
    );
    let org_response = state
        .http
        .get(org_url)
        .bearer_auth(token)
        .headers(github_headers())
        .send()
        .await?;
    if org_response.status() != reqwest::StatusCode::NOT_FOUND {
        return org_response.error_for_status()?.json().await;
    }
    let user_url = github_api_url(
        &state.github_api_base_url,
        &format!(
            "/users/{}/packages?package_type=container&per_page=100",
            github_path_segment(owner)
        ),
    );
    state
        .http
        .get(user_url)
        .bearer_auth(token)
        .headers(github_headers())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
}

async fn github_container_package_versions(
    state: &AppState,
    token: &str,
    owner: &str,
    package_name: &str,
) -> std::result::Result<Vec<GithubPackageVersionApi>, reqwest::Error> {
    let org_url = github_api_url(
        &state.github_api_base_url,
        &format!(
            "/orgs/{}/packages/container/{}/versions?per_page=30",
            github_path_segment(owner),
            github_path_segment(package_name)
        ),
    );
    let org_response = state
        .http
        .get(org_url)
        .bearer_auth(token)
        .headers(github_headers())
        .send()
        .await?;
    if org_response.status() != reqwest::StatusCode::NOT_FOUND {
        return org_response.error_for_status()?.json().await;
    }
    let user_url = github_api_url(
        &state.github_api_base_url,
        &format!(
            "/users/{}/packages/container/{}/versions?per_page=30",
            github_path_segment(owner),
            github_path_segment(package_name)
        ),
    );
    state
        .http
        .get(user_url)
        .bearer_auth(token)
        .headers(github_headers())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
}

impl QueuedDelivery {
    fn into_event(self) -> RelayEvent {
        RelayEvent {
            sequence: self.sequence,
            delivery_id: self.delivery_id,
            kind: RelayEventKind::from_github_event(&self.event_name),
            payload: self.payload,
        }
    }
}

fn relay_response(id: String, result: RelayResult<Value>) -> RelayResponse {
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
                code: error_code(&error).to_string(),
                message: error.to_string(),
            }),
        },
    }
}

fn error_code(error: &RelayErrorKind) -> &'static str {
    match error {
        RelayErrorKind::InvalidInput(_) => "invalid_input",
        RelayErrorKind::Db(_) => "database",
        RelayErrorKind::Json(_) => "json",
        RelayErrorKind::Http(_) => "http",
        RelayErrorKind::Jwt(_) => "jwt",
    }
}

async fn bootstrap_github_app_config(
    store: &RelayStore,
    http: &reqwest::Client,
    public_url: &str,
    github_api_base_url: &str,
    github_web_base_url: &str,
) -> Result<()> {
    let env_config = github_app_config_from_env()?;
    let stored_config = store.github_app_config()?;
    let Some(mut config) = merge_github_app_config(env_config, stored_config) else {
        return Ok(());
    };
    hydrate_github_app_metadata(http, github_api_base_url, &mut config)
        .await
        .context("loading GitHub App metadata")?;
    let mut generated_secret = false;
    if config.webhook_secret.trim().is_empty() {
        let webhook_secret = Uuid::new_v4().to_string();
        config.webhook_secret = webhook_secret;
        match update_github_app_hook_config(http, github_api_base_url, public_url, &config)
            .await
            .context("resetting GitHub App webhook secret")?
        {
            GithubHookReset::Updated => {
                generated_secret = true;
            }
            GithubHookReset::Missing => {
                config.webhook_secret = String::new();
                warn!(
                    app_id = %config.app_id,
                    "GitHub App webhook config was not found; enable the app webhook in GitHub settings or manifest flow before webhook signature verification can work"
                );
            }
        }
    }
    store.save_github_app_config(&config)?;
    info!(
        app_id = %config.app_id,
        app_slug = config.app_slug.as_deref().unwrap_or(""),
        generated_webhook_secret = generated_secret,
        github_web_base_url = %github_web_base_url,
        "loaded GitHub App config"
    );
    Ok(())
}

async fn hydrate_github_app_metadata(
    http: &reqwest::Client,
    github_api_base_url: &str,
    config: &mut GithubAppConfig,
) -> RelayResult<()> {
    if github_app_config_has_metadata(config) {
        return Ok(());
    }
    let jwt = github_app_jwt_for_config(config)?;
    let url = github_api_url(github_api_base_url, "/app");
    let response = http
        .get(url)
        .bearer_auth(jwt)
        .headers(github_headers())
        .send()
        .await?;
    let app = decode_github_response::<GithubAppApi>(response, "get GitHub App metadata").await?;
    apply_github_app_metadata(config, app);
    Ok(())
}

fn github_app_config_has_metadata(config: &GithubAppConfig) -> bool {
    config
        .app_slug
        .as_deref()
        .is_some_and(|slug| !is_placeholder_github_app_slug(slug))
        && config.app_html_url.is_some()
        && config.owner_login.is_some()
        && config.owner_type.is_some()
}

fn apply_github_app_metadata(config: &mut GithubAppConfig, app: GithubAppApi) {
    if config
        .app_slug
        .as_deref()
        .is_none_or(is_placeholder_github_app_slug)
    {
        config.app_slug = Some(app.slug);
    }
    if config.app_html_url.is_none() {
        config.app_html_url = Some(app.html_url);
    }
    if let Some(owner) = app.owner {
        if config.owner_login.is_none() {
            config.owner_login = Some(owner.login);
        }
        if config.owner_type.is_none() {
            config.owner_type = Some(owner.account_type);
        }
    }
}

fn merge_github_app_config(
    env_config: Option<GithubAppConfig>,
    stored_config: Option<GithubAppConfig>,
) -> Option<GithubAppConfig> {
    match (env_config, stored_config) {
        (None, stored) => stored,
        (Some(mut env), stored) => {
            if env.webhook_secret.trim().is_empty() {
                env.webhook_secret = stored
                    .as_ref()
                    .map(|config| config.webhook_secret.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .unwrap_or_default();
            }
            if env.app_html_url.is_none() {
                env.app_html_url = stored
                    .as_ref()
                    .and_then(|config| config.app_html_url.clone());
            }
            if env.owner_login.is_none() {
                env.owner_login = stored
                    .as_ref()
                    .and_then(|config| config.owner_login.clone());
            }
            if env.owner_type.is_none() {
                env.owner_type = stored.as_ref().and_then(|config| config.owner_type.clone());
            }
            Some(env)
        }
    }
}

async fn update_github_app_hook_config(
    http: &reqwest::Client,
    github_api_base_url: &str,
    public_url: &str,
    config: &GithubAppConfig,
) -> RelayResult<GithubHookReset> {
    if config.app_id.trim().is_empty() || config.private_key.trim().is_empty() {
        return Err(RelayErrorKind::InvalidInput(
            "GitHub App ID and private key are required to reset webhook secret".to_string(),
        ));
    }
    let jwt = github_app_jwt_for_config(config)?;
    let current_url = github_api_url(github_api_base_url, "/app/hook/config");
    let current = http
        .get(&current_url)
        .bearer_auth(&jwt)
        .headers(github_headers())
        .send()
        .await?;
    if current.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(GithubHookReset::Missing);
    }
    let current = decode_github_response::<GithubAppHookConfigResponse>(
        current,
        "get GitHub App webhook config",
    )
    .await?;
    if current
        .url
        .as_deref()
        .is_some_and(|url| url.trim().is_empty())
    {
        return Ok(GithubHookReset::Missing);
    }
    let url = github_api_url(github_api_base_url, "/app/hook/config");
    let body = github_app_hook_config_request(public_url, &config.webhook_secret);
    let response = http
        .patch(url)
        .bearer_auth(&jwt)
        .headers(github_headers())
        .json(&body)
        .send()
        .await?;
    decode_github_response::<Value>(response, "update GitHub App webhook config").await?;
    Ok(GithubHookReset::Updated)
}

fn github_app_hook_config_request(public_url: &str, secret: &str) -> GithubAppHookConfigRequest {
    GithubAppHookConfigRequest {
        url: format!("{}/github/webhook", public_url.trim_end_matches('/')),
        content_type: "json",
        insecure_ssl: "0",
        secret: secret.to_string(),
    }
}

fn github_app_config_from_env() -> Result<Option<GithubAppConfig>> {
    let app_id = env::var("MAI_RELAY_GITHUB_APP_ID").ok();
    let private_key = env::var("MAI_RELAY_GITHUB_APP_PRIVATE_KEY").ok();
    let private_key_path = env::var("MAI_RELAY_GITHUB_APP_PRIVATE_KEY_PATH").ok();
    let slug = env::var("MAI_RELAY_GITHUB_APP_SLUG").ok();

    let any_present = [
        app_id.as_deref(),
        private_key.as_deref(),
        private_key_path.as_deref(),
        slug.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| !value.trim().is_empty());
    if !any_present {
        return Ok(None);
    }

    let app_id = app_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .context("MAI_RELAY_GITHUB_APP_ID is required when relay GitHub App env is configured")?;
    let private_key = match private_key
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        Some(private_key) => private_key,
        None => {
            let path = private_key_path
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .context("MAI_RELAY_GITHUB_APP_PRIVATE_KEY or MAI_RELAY_GITHUB_APP_PRIVATE_KEY_PATH is required")?;
            std::fs::read_to_string(&path)
                .with_context(|| format!("reading MAI_RELAY_GITHUB_APP_PRIVATE_KEY_PATH {path}"))?
        }
    };
    let app_slug = slug
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && !is_placeholder_github_app_slug(value));
    Ok(Some(GithubAppConfig {
        app_id,
        private_key,
        webhook_secret: String::new(),
        app_slug,
        app_html_url: optional_env("MAI_RELAY_GITHUB_APP_HTML_URL"),
        owner_login: optional_env("MAI_RELAY_GITHUB_APP_OWNER_LOGIN"),
        owner_type: optional_env("MAI_RELAY_GITHUB_APP_OWNER_TYPE"),
    }))
}

fn optional_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn is_placeholder_github_app_slug(value: &str) -> bool {
    value.trim() == "github-app-slug"
}

fn parse_params<T>(params: Value) -> impl std::future::Future<Output = RelayResult<T>>
where
    T: DeserializeOwned,
{
    async move { Ok(serde_json::from_value(params)?) }
}

fn to_value<T>(value: T) -> RelayResult<Value>
where
    T: Serialize,
{
    Ok(serde_json::to_value(value)?)
}

fn verify_signature(secret: &str, body: &[u8], signature: &str) -> bool {
    let Some(hex) = signature.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = decode_hex(hex) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

fn decode_hex(value: &str) -> std::result::Result<Vec<u8>, ()> {
    if !value.len().is_multiple_of(2) {
        return Err(());
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    for chunk in value.as_bytes().chunks(2) {
        let high = hex_value(chunk[0])?;
        let low = hex_value(chunk[1])?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

fn hex_value(byte: u8) -> std::result::Result<u8, ()> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(()),
    }
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn github_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(USER_AGENT, HeaderValue::from_static("mai-team-relay"));
    headers.insert(
        "X-GitHub-Api-Version",
        HeaderValue::from_static(GITHUB_API_VERSION),
    );
    headers
}

async fn decode_github_response<T>(response: reqwest::Response, action: &str) -> RelayResult<T>
where
    T: DeserializeOwned,
{
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if status.is_success() {
        return Ok(serde_json::from_str(&text)?);
    }
    let message = serde_json::from_str::<GithubErrorResponse>(&text)
        .ok()
        .and_then(|error| error.message)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| text.chars().take(300).collect());
    Err(RelayErrorKind::InvalidInput(format!(
        "{action} failed with {status}: {message}"
    )))
}

fn github_api_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

fn github_repository_summary(repository: GithubRepositoryApi) -> GithubRepositorySummary {
    GithubRepositorySummary {
        id: repository.id,
        owner: repository.owner.login,
        name: repository.name,
        full_name: repository.full_name,
        private: repository.private,
        clone_url: repository.clone_url,
        html_url: repository.html_url,
        default_branch: repository.default_branch,
    }
}

fn github_app_settings_response(
    config: &GithubAppConfig,
    api_base_url: &str,
    web_base_url: &str,
    state: Option<&str>,
) -> GithubAppSettingsResponse {
    GithubAppSettingsResponse {
        app_id: Some(config.app_id.clone()),
        base_url: api_base_url.to_string(),
        has_private_key: !config.private_key.trim().is_empty(),
        app_slug: config.app_slug.clone(),
        app_html_url: config.app_html_url.clone(),
        owner_login: config.owner_login.clone(),
        owner_type: config.owner_type.clone(),
        install_url: config
            .app_slug
            .as_deref()
            .map(|slug| github_app_install_url(web_base_url, slug, state)),
    }
}

fn github_app_install_url(web_base_url: &str, app_slug: &str, state: Option<&str>) -> String {
    let base = format!(
        "{}/apps/{}/installations/new",
        web_base_url.trim_end_matches('/'),
        app_slug
    );
    match state {
        Some(state) => format!("{base}?state={state}"),
        None => base,
    }
}

fn local_return_url(origin: &str, return_hash: &str, params: &str) -> RelayResult<String> {
    let origin = sanitize_origin(origin)?;
    let hash = if return_hash.trim().is_empty() {
        "#projects"
    } else {
        return_hash.trim()
    };
    let hash = if hash.starts_with('#') {
        hash.to_string()
    } else {
        format!("#{hash}")
    };
    let separator = if hash.contains('?') { '&' } else { '?' };
    Ok(format!("{origin}/{hash}{separator}{params}"))
}

fn sanitize_origin(value: &str) -> RelayResult<String> {
    let value = value.trim().trim_end_matches('/');
    if value.starts_with("http://") || value.starts_with("https://") {
        Ok(value.to_string())
    } else {
        Err(RelayErrorKind::InvalidInput(
            "origin must start with http:// or https://".to_string(),
        ))
    }
}

fn is_valid_github_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn is_valid_manifest_code(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn github_package_belongs_to_repo(package: &GithubPackageApi, repository_full_name: &str) -> bool {
    package.repository.as_ref().is_some_and(|repository| {
        repository
            .full_name
            .eq_ignore_ascii_case(repository_full_name)
    })
}

fn repository_package_summary(
    owner: &str,
    package: GithubPackageApi,
    versions: Vec<GithubPackageVersionApi>,
) -> Option<RepositoryPackageSummary> {
    let tag = preferred_container_tag(&versions)?;
    let image = format!("ghcr.io/{}/{}:{}", owner, package.name, tag);
    Some(RepositoryPackageSummary {
        name: package.name,
        image,
        tag,
        html_url: package.html_url,
    })
}

fn github_packages_read_error(status: Option<reqwest::StatusCode>) -> bool {
    matches!(
        status,
        Some(
            reqwest::StatusCode::BAD_REQUEST
                | reqwest::StatusCode::FORBIDDEN
                | reqwest::StatusCode::NOT_FOUND
        )
    )
}

fn preferred_container_tag(versions: &[GithubPackageVersionApi]) -> Option<String> {
    let mut first_tag = None;
    for version in versions {
        for tag in &version.metadata.container.tags {
            let tag = tag.trim();
            if tag.is_empty() {
                continue;
            }
            if tag == "latest" {
                return Some(tag.to_string());
            }
            if first_tag.is_none() {
                first_tag = Some(tag.to_string());
            }
        }
    }
    first_tag
}

fn github_path_segment(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn github_app_manifest(
    redirect_url: &str,
    setup_url: &str,
    webhook_url: &str,
    webhook_secret: &str,
) -> Value {
    json!({
        "name": format!("Mai Team {}", Uuid::new_v4().to_string().split('-').next().unwrap_or("project")),
        "url": "https://github.com",
        "redirect_url": redirect_url,
        "callback_urls": [redirect_url],
        "setup_url": setup_url,
        "public": false,
        "default_permissions": {
            "contents": "write",
            "pull_requests": "write",
            "issues": "write",
            "checks": "read",
            "statuses": "read",
            "metadata": "read"
        },
        "default_events": [
            "pull_request",
            "push",
            "check_run",
            "check_suite",
            "installation",
            "installation_repositories"
        ],
        "hook_attributes": {
            "url": webhook_url,
            "active": true
        },
        "webhook_secret": webhook_secret
    })
}

fn callback_page(success: bool, title: &str, message: &str) -> Response {
    let status = if success {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    let accent = if success { "#0b7a53" } else { "#b42318" };
    let title = html_escape(title);
    let message = html_escape(message);
    (
        status,
        Html(format!(
            "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
             <body style=\"font-family: system-ui, sans-serif; margin: 3rem; line-height: 1.5\">\
             <h1 style=\"color:{accent}\">{title}</h1><p>{message}</p></body>"
        )),
    )
        .into_response()
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

struct RelayStore {
    path: PathBuf,
}

impl RelayStore {
    fn open(path: PathBuf) -> RelayResult<Self> {
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

    fn next_sequence(&self) -> RelayResult<u64> {
        let conn = self.connection()?;
        let sequence: Option<i64> = conn
            .query_row("SELECT MAX(sequence) FROM webhook_deliveries", [], |row| {
                row.get(0)
            })
            .optional()?
            .flatten();
        Ok(sequence.unwrap_or(0).saturating_add(1) as u64)
    }

    fn queued_count(&self) -> RelayResult<u64> {
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

    fn save_github_app_config(&self, config: &GithubAppConfig) -> RelayResult<()> {
        self.set_setting("github_app_config", &serde_json::to_string(config)?)
    }

    fn github_app_config(&self) -> RelayResult<Option<GithubAppConfig>> {
        self.get_setting("github_app_config")?
            .map(|value| Ok(serde_json::from_str(&value)?))
            .transpose()
    }

    fn save_manifest_state(&self, state: &ManifestState, webhook_secret: &str) -> RelayResult<()> {
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

    fn take_manifest_state(&self, state: &str) -> RelayResult<(ManifestState, String)> {
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

    fn save_installation_state(&self, state: &InstallationState) -> RelayResult<()> {
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

    fn take_installation_state(&self, state: &str) -> RelayResult<InstallationState> {
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

    fn take_latest_installation_state(&self) -> RelayResult<InstallationState> {
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

    fn insert_delivery(
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

    fn list_unacked_deliveries(&self) -> RelayResult<Vec<QueuedDelivery>> {
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

    fn ack_delivery(&self, delivery_id: &str) -> RelayResult<()> {
        let conn = self.connection()?;
        conn.execute(
            "UPDATE webhook_deliveries SET acked_at = ?2 WHERE delivery_id = ?1",
            params![delivery_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    fn cached_token(
        &self,
        installation_id: u64,
        repository_id: Option<u64>,
    ) -> RelayResult<Option<RelayGithubInstallationTokenResponse>> {
        let conn = self.connection()?;
        let cache_key = token_cache_key(installation_id, repository_id);
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

    fn save_cached_token(
        &self,
        installation_id: u64,
        repository_id: Option<u64>,
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
                token_cache_key(installation_id, repository_id),
                installation_id as i64,
                repository_id.map(|id| id as i64),
                token.token,
                token.expires_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }
}

fn token_cache_key(installation_id: u64, repository_id: Option<u64>) -> String {
    format!(
        "{installation_id}:{}",
        repository_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "all".to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_validation_accepts_expected_signature() {
        let secret = "secret";
        let body = br#"{"ok":true}"#;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac");
        mac.update(body);
        let signature = format!("sha256={}", hex_encode(&mac.finalize().into_bytes()));

        assert!(verify_signature(secret, body, &signature));
        assert!(!verify_signature(secret, body, "sha256=00"));
        assert!(!verify_signature(secret, body, ""));
    }

    #[test]
    fn manifest_uses_active_webhook_and_events() {
        let manifest = github_app_manifest(
            "https://relay.example/github/app-manifest/callback",
            "https://relay.example/github/app-installation/callback",
            "https://relay.example/github/webhook",
            "secret",
        );

        assert_eq!(manifest["hook_attributes"]["active"], true);
        assert_eq!(
            manifest["hook_attributes"]["url"],
            "https://relay.example/github/webhook"
        );
        assert_eq!(manifest["default_permissions"]["contents"], "write");
        assert_eq!(manifest["default_events"][0], "pull_request");
        assert_eq!(manifest["webhook_secret"], "secret");
    }

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
    fn installation_state_round_trips_and_return_url_is_hash_based() {
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
        assert_eq!(
            local_return_url(
                &state.origin,
                &state.return_hash,
                "github-app=installed&installation_id=42"
            )
            .expect("url"),
            "http://127.0.0.1:8080/#projects?github-app=installed&installation_id=42"
        );
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
    fn app_settings_builds_stateful_install_url() {
        let config = GithubAppConfig {
            app_id: "123".to_string(),
            private_key: "pem".to_string(),
            webhook_secret: "secret".to_string(),
            app_slug: Some("mai-test".to_string()),
            app_html_url: Some("https://github.com/apps/mai-test".to_string()),
            owner_login: Some("owner".to_string()),
            owner_type: Some("User".to_string()),
        };
        let settings = github_app_settings_response(
            &config,
            "https://api.github.com",
            "https://github.com",
            Some("state-1"),
        );
        assert_eq!(settings.app_id.as_deref(), Some("123"));
        assert!(settings.has_private_key);
        assert_eq!(
            settings.install_url.as_deref(),
            Some("https://github.com/apps/mai-test/installations/new?state=state-1")
        );
    }

    #[test]
    fn github_app_config_deserializes_missing_webhook_secret_as_empty() {
        let config: GithubAppConfig = serde_json::from_value(json!({
            "app_id": "123",
            "private_key": "pem",
            "app_slug": "mai-test"
        }))
        .expect("config");

        assert_eq!(config.webhook_secret, "");
    }

    #[test]
    fn env_config_preserves_stored_webhook_secret_when_merged() {
        let env = GithubAppConfig {
            app_id: "456".to_string(),
            private_key: "env-pem".to_string(),
            webhook_secret: String::new(),
            app_slug: Some("env-slug".to_string()),
            app_html_url: None,
            owner_login: None,
            owner_type: None,
        };
        let stored = GithubAppConfig {
            app_id: "123".to_string(),
            private_key: "stored-pem".to_string(),
            webhook_secret: "stored-secret".to_string(),
            app_slug: Some("stored-slug".to_string()),
            app_html_url: Some("https://github.com/apps/stored".to_string()),
            owner_login: Some("owner".to_string()),
            owner_type: Some("User".to_string()),
        };

        let merged = merge_github_app_config(Some(env), Some(stored)).expect("merged");
        assert_eq!(merged.app_id, "456");
        assert_eq!(merged.private_key, "env-pem");
        assert_eq!(merged.webhook_secret, "stored-secret");
        assert_eq!(merged.app_slug.as_deref(), Some("env-slug"));
        assert_eq!(merged.owner_login.as_deref(), Some("owner"));
    }

    #[test]
    fn github_app_hook_config_request_uses_public_webhook_url() {
        let request = github_app_hook_config_request("https://relay.example/", "secret-1");
        assert_eq!(request.url, "https://relay.example/github/webhook");
        assert_eq!(request.content_type, "json");
        assert_eq!(request.insecure_ssl, "0");
        assert_eq!(request.secret, "secret-1");
    }

    #[test]
    fn github_app_api_metadata_fills_config_fields() {
        let app: GithubAppApi = serde_json::from_value(json!({
            "slug": "mai-team-app",
            "html_url": "https://github.com/apps/mai-team-app",
            "owner": {
                "login": "mai-team",
                "type": "Organization"
            }
        }))
        .expect("app");

        let mut config = GithubAppConfig {
            app_id: "123".to_string(),
            private_key: "pem".to_string(),
            webhook_secret: "secret".to_string(),
            app_slug: Some("github-app-slug".to_string()),
            app_html_url: None,
            owner_login: None,
            owner_type: None,
        };
        assert!(!github_app_config_has_metadata(&config));
        apply_github_app_metadata(&mut config, app);

        assert_eq!(config.app_slug.as_deref(), Some("mai-team-app"));
        assert_eq!(
            config.app_html_url.as_deref(),
            Some("https://github.com/apps/mai-team-app")
        );
        assert_eq!(config.owner_login.as_deref(), Some("mai-team"));
        assert_eq!(config.owner_type.as_deref(), Some("Organization"));
        assert!(github_app_config_has_metadata(&config));
    }

    #[test]
    fn github_packages_bad_request_is_read_warning() {
        assert!(github_packages_read_error(Some(
            reqwest::StatusCode::BAD_REQUEST
        )));
        assert!(github_packages_read_error(Some(
            reqwest::StatusCode::FORBIDDEN
        )));
        assert!(github_packages_read_error(Some(
            reqwest::StatusCode::NOT_FOUND
        )));
        assert!(!github_packages_read_error(Some(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        )));
    }

    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
