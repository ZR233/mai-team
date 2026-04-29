use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use mai_docker::DockerClient;
use mai_model::ResponsesClient;
use mai_protocol::{
    AgentId, CreateAgentRequest, CreateAgentResponse, ErrorResponse, FileUploadRequest,
    FileUploadResponse, ProvidersConfigRequest, ProvidersResponse, SendMessageRequest,
    SendMessageResponse, ServiceEvent, ToolTraceDetail,
};
use mai_runtime::{AgentRuntime, RuntimeConfig, RuntimeError};
use mai_store::ConfigStore;
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

#[derive(Clone)]
struct AppState {
    runtime: Arc<AgentRuntime>,
    store: Arc<ConfigStore>,
    token: String,
}

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/static"]
struct StaticAssets;

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "missing or invalid bearer token".to_string(),
        }
    }
}

impl From<RuntimeError> for ApiError {
    fn from(value: RuntimeError) -> Self {
        let status = match value {
            RuntimeError::AgentNotFound(_) => StatusCode::NOT_FOUND,
            RuntimeError::ToolTraceNotFound { .. } => StatusCode::NOT_FOUND,
            RuntimeError::AgentBusy(_) => StatusCode::CONFLICT,
            RuntimeError::InvalidInput(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: value.to_string(),
        }
    }
}

impl From<mai_store::StoreError> for ApiError {
    fn from(value: mai_store::StoreError) -> Self {
        let status = match value {
            mai_store::StoreError::InvalidConfig(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: value.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[derive(Debug, Deserialize)]
struct DownloadQuery {
    path: String,
}

#[derive(Debug, Deserialize, Default)]
struct EventQuery {
    token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mai_server=info,mai_runtime=info,tower_http=info".into()),
        )
        .init();

    let (token, generated_token) = load_or_generate_token();
    let api_key = env::var("OPENAI_API_KEY").ok();
    let base_url =
        env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-5.2".to_string());
    let db_path = env::var("MAI_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or(ConfigStore::default_path()?);
    let image = env::var("MAI_AGENT_BASE_IMAGE").unwrap_or_else(|_| "ubuntu:24.04".to_string());
    let bind = env::var("MAI_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let addr: SocketAddr = bind.parse().context("invalid MAI_BIND_ADDR")?;

    let docker = DockerClient::new(image);
    let docker_version = docker.check_available().await?;
    info!("docker available: {docker_version}");
    let cleaned = docker.cleanup_stale_containers().await?;
    if !cleaned.is_empty() {
        info!(count = cleaned.len(), "removed stale mai-team containers");
    }

    let store = Arc::new(ConfigStore::open(db_path).await?);
    store
        .seed_default_provider_from_env(api_key, base_url, model)
        .await?;
    if let Some(home) = dirs::home_dir() {
        let legacy_path = home.join(".mai-team").join("config.toml");
        if store.import_legacy_toml_once(legacy_path).await? {
            info!("imported legacy MCP config into SQLite");
        }
    }

    let model = ResponsesClient::new();
    let runtime = AgentRuntime::new(
        docker,
        model,
        Arc::clone(&store),
        RuntimeConfig {
            repo_root: env::current_dir()?,
        },
    )
    .await?;
    let state = Arc::new(AppState {
        runtime,
        store,
        token: token.clone(),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/providers", get(get_providers).put(save_providers))
        .route("/events", get(events))
        .route("/agents", get(list_agents).post(create_agent))
        .route(
            "/agents/{id}",
            get(get_agent).delete(delete_agent).post(cancel_agent_colon),
        )
        .route("/agents/{id}/messages", post(send_message))
        .route("/agents/{id}/tool-calls/{call_id}", get(get_tool_trace))
        .route("/agents/{id}/files:upload", post(upload_file))
        .route("/agents/{id}/files:download", get(download_file))
        .route("/agents/{id}/cancel", post(cancel_agent))
        .fallback(get(static_fallback))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    if generated_token {
        println!("Mai Team generated token: {token}");
    } else {
        println!("Mai Team token: {token}");
    }
    println!("Open http://{addr}/?token={token}");
    info!("mai-team listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Response {
    embedded_asset_response("index.html", true)
}

async fn static_fallback(uri: Uri) -> Response {
    embedded_asset_response(uri.path().trim_start_matches('/'), true)
}

fn embedded_asset_response(path: &str, fallback_index: bool) -> Response {
    let asset_path = if path.is_empty() { "index.html" } else { path };
    let (served_path, asset) = match StaticAssets::get(asset_path) {
        Some(asset) => (asset_path, asset),
        None if fallback_index && !asset_path.contains('.') => {
            match StaticAssets::get("index.html") {
                Some(asset) => ("index.html", asset),
                None => {
                    return (StatusCode::NOT_FOUND, "embedded index.html not found")
                        .into_response();
                }
            }
        }
        None => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let content_type = mime_guess::from_path(served_path)
        .first_or_octet_stream()
        .essence_str()
        .to_string();

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(asset.data.into_owned()))
        .expect("embedded static response")
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

async fn get_providers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> std::result::Result<Json<ProvidersResponse>, ApiError> {
    authorize(&state, &headers, None)?;
    Ok(Json(state.store.providers_response().await?))
}

async fn save_providers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<ProvidersConfigRequest>,
) -> std::result::Result<Json<ProvidersResponse>, ApiError> {
    authorize(&state, &headers, None)?;
    state.store.save_providers(request).await?;
    Ok(Json(state.store.providers_response().await?))
}

async fn list_agents(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> std::result::Result<Json<Vec<mai_protocol::AgentSummary>>, ApiError> {
    authorize(&state, &headers, None)?;
    Ok(Json(state.runtime.list_agents().await))
}

async fn create_agent(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<CreateAgentRequest>,
) -> std::result::Result<Json<CreateAgentResponse>, ApiError> {
    authorize(&state, &headers, None)?;
    let agent = state.runtime.create_agent(request).await?;
    Ok(Json(CreateAgentResponse { agent }))
}

async fn get_agent(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<AgentId>,
) -> std::result::Result<Json<mai_protocol::AgentDetail>, ApiError> {
    authorize(&state, &headers, None)?;
    Ok(Json(state.runtime.get_agent(id).await?))
}

async fn send_message(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<AgentId>,
    Json(request): Json<SendMessageRequest>,
) -> std::result::Result<Json<SendMessageResponse>, ApiError> {
    authorize(&state, &headers, None)?;
    let turn_id = state
        .runtime
        .send_message(id, request.message, request.skill_mentions)
        .await?;
    Ok(Json(SendMessageResponse { turn_id }))
}

async fn get_tool_trace(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, call_id)): Path<(AgentId, String)>,
) -> std::result::Result<Json<ToolTraceDetail>, ApiError> {
    authorize(&state, &headers, None)?;
    Ok(Json(state.runtime.tool_trace(id, call_id).await?))
}

async fn upload_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<AgentId>,
    Json(request): Json<FileUploadRequest>,
) -> std::result::Result<Json<FileUploadResponse>, ApiError> {
    authorize(&state, &headers, None)?;
    let bytes = state
        .runtime
        .upload_file(id, request.path.clone(), request.content_base64)
        .await?;
    Ok(Json(FileUploadResponse {
        path: request.path,
        bytes,
    }))
}

async fn download_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<AgentId>,
    Query(query): Query<DownloadQuery>,
) -> std::result::Result<Response, ApiError> {
    authorize(&state, &headers, None)?;
    let bytes = state.runtime.download_file_tar(id, query.path).await?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-tar")
        .body(Body::from(bytes))
        .expect("response builder"))
}

async fn cancel_agent(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<AgentId>,
) -> std::result::Result<StatusCode, ApiError> {
    authorize(&state, &headers, None)?;
    state.runtime.cancel_agent(id).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn cancel_agent_colon(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> std::result::Result<StatusCode, ApiError> {
    authorize(&state, &headers, None)?;
    let id = id.strip_suffix(":cancel").unwrap_or(&id);
    let id = id.parse::<AgentId>().map_err(|err| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: format!("invalid agent id: {err}"),
    })?;
    state.runtime.cancel_agent(id).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn delete_agent(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<AgentId>,
) -> std::result::Result<StatusCode, ApiError> {
    authorize(&state, &headers, None)?;
    state.runtime.delete_agent(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn events(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<EventQuery>,
) -> std::result::Result<
    Sse<impl futures::Stream<Item = std::result::Result<Event, Infallible>>>,
    ApiError,
> {
    authorize(&state, &headers, query.token.as_deref())?;
    let stream = BroadcastStream::new(state.runtime.subscribe()).filter_map(|event| async move {
        match event {
            Ok(event) => Some(Ok(sse_event(event))),
            Err(_) => None,
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> std::result::Result<(), ApiError> {
    if query_token.is_some_and(|token| token == state.token) {
        return Ok(());
    }
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return Err(ApiError::unauthorized());
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return Err(ApiError::unauthorized());
    };
    if token == state.token {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

fn sse_event(event: ServiceEvent) -> Event {
    Event::default()
        .id(event.sequence.to_string())
        .event(event_name(&event))
        .json_data(event)
        .unwrap_or_else(|_| Event::default().data("{}"))
}

fn event_name(event: &ServiceEvent) -> &'static str {
    match &event.kind {
        mai_protocol::ServiceEventKind::AgentCreated { .. } => "agent_created",
        mai_protocol::ServiceEventKind::AgentStatusChanged { .. } => "agent_status_changed",
        mai_protocol::ServiceEventKind::AgentDeleted { .. } => "agent_deleted",
        mai_protocol::ServiceEventKind::TurnStarted { .. } => "turn_started",
        mai_protocol::ServiceEventKind::TurnCompleted { .. } => "turn_completed",
        mai_protocol::ServiceEventKind::ToolStarted { .. } => "tool_started",
        mai_protocol::ServiceEventKind::ToolCompleted { .. } => "tool_completed",
        mai_protocol::ServiceEventKind::AgentMessage { .. } => "agent_message",
        mai_protocol::ServiceEventKind::Error { .. } => "error",
    }
}

fn load_or_generate_token() -> (String, bool) {
    match env::var("MAI_TEAM_TOKEN") {
        Ok(token) if !token.trim().is_empty() => (token, false),
        _ => (
            format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            true,
        ),
    }
}
