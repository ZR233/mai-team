use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use mai_docker::DockerClient;
use mai_model::ResponsesClient;
use mai_protocol::{
    AgentId, CreateAgentRequest, CreateAgentResponse, ErrorResponse, FileUploadRequest,
    FileUploadResponse, ProvidersConfigRequest, ProvidersResponse, SendMessageRequest,
    SendMessageResponse, ServiceEvent,
};
use mai_runtime::{AgentRuntime, RuntimeConfig, RuntimeError};
use mai_store::ConfigStore;
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

    let store = Arc::new(ConfigStore::open(db_path)?);
    store.seed_default_provider_from_env(api_key, base_url, model)?;
    if let Some(home) = dirs::home_dir() {
        let legacy_path = home.join(".mai-team").join("config.toml");
        if store.import_legacy_toml_once(legacy_path)? {
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
    )?;
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
        .route("/agents/{id}/files:upload", post(upload_file))
        .route("/agents/{id}/files:download", get(download_file))
        .route("/agents/{id}/cancel", post(cancel_agent))
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

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

async fn get_providers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> std::result::Result<Json<ProvidersResponse>, ApiError> {
    authorize(&state, &headers, None)?;
    Ok(Json(state.store.providers_response()?))
}

async fn save_providers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<ProvidersConfigRequest>,
) -> std::result::Result<Json<ProvidersResponse>, ApiError> {
    authorize(&state, &headers, None)?;
    state.store.save_providers(request)?;
    Ok(Json(state.store.providers_response()?))
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

const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Mai Team</title>
  <style>
    :root { color-scheme: light dark; font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
    body { margin: 0; background: #f6f7f9; color: #172033; }
    header { display: flex; align-items: center; gap: 12px; padding: 14px 18px; background: #fff; border-bottom: 1px solid #d9dee8; }
    h1 { font-size: 18px; margin: 0; }
    h2 { font-size: 15px; margin: 0 0 10px; }
    main { display: grid; grid-template-columns: 340px 1fr; min-height: calc(100vh - 56px); }
    aside { border-right: 1px solid #d9dee8; background: #fff; overflow: hidden; display: flex; flex-direction: column; }
    section { padding: 16px; overflow: auto; }
    input, button, textarea, select { font: inherit; }
    input, textarea, select { border: 1px solid #bdc6d5; border-radius: 6px; padding: 8px; background: #fff; color: #172033; min-width: 0; }
    textarea { resize: vertical; }
    button { border: 1px solid #2f6fed; border-radius: 6px; padding: 8px 10px; background: #2f6fed; color: #fff; cursor: pointer; }
    button.secondary { background: #fff; color: #2f4b7c; border-color: #bdc6d5; }
    button.danger { background: #b42318; border-color: #b42318; }
    .toolbar { display: flex; gap: 8px; align-items: center; margin-left: auto; }
    .tabs { display: flex; gap: 6px; }
    .tab { background: #fff; color: #2f4b7c; border-color: #bdc6d5; }
    .tab.active { background: #2f6fed; color: #fff; border-color: #2f6fed; }
    .sidebar-header { display: flex; align-items: center; justify-content: space-between; padding: 14px 16px; border-bottom: 1px solid #eef1f6; flex-shrink: 0; }
    .sidebar-header h2 { margin: 0; font-size: 14px; font-weight: 700; }
    .sidebar-header button { padding: 5px 12px; font-size: 12px; }
    .agent-list { flex: 1; overflow-y: auto; }
    .agent-empty-sidebar { text-align: center; padding: 40px 16px; color: #8896a8; font-size: 13px; }
    .agent-item { display: flex; align-items: center; gap: 10px; padding: 12px 16px; border-bottom: 1px solid #f0f2f5; cursor: pointer; transition: background 0.15s; }
    .agent-item:hover { background: #f8f9fb; }
    .agent-item.active { background: #eef4ff; border-left: 3px solid #2f6fed; padding-left: 13px; }
    .agent-avatar { width: 36px; height: 36px; border-radius: 10px; background: linear-gradient(135deg, #eef4ff, #dce9ff); display: flex; align-items: center; justify-content: center; font-weight: 700; font-size: 14px; color: #2f6fed; flex-shrink: 0; }
    .agent-info { flex: 1; min-width: 0; }
    .agent-info .name { font-weight: 600; font-size: 14px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .agent-info .meta { color: #8896a8; font-size: 12px; margin-top: 2px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .status-dot { width: 8px; height: 8px; border-radius: 50%; flex-shrink: 0; }
    .status-dot.idle { background: #94a3b8; }
    .status-dot.running { background: #22c55e; box-shadow: 0 0 0 2px rgba(34,197,94,0.2); animation: pulse 2s ease-in-out infinite; }
    .status-dot.error { background: #ef4444; }
    .status-dot.pending { background: #f59e0b; animation: pulse 1.5s ease-in-out infinite; }
    .status-dot.done { background: #6366f1; }
    @keyframes pulse { 0%,100% { opacity: 1; } 50% { opacity: 0.4; } }
    .grid { display: grid; gap: 12px; }
    .panel { background: #fff; border: 1px solid #d9dee8; border-radius: 8px; padding: 12px; }
    .row { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
    .form { display: grid; gap: 8px; grid-template-columns: repeat(2, minmax(0, 1fr)); }
    .form .full { grid-column: 1 / -1; }
    label.field { display: grid; gap: 5px; color: #3d4758; font-size: 12px; }
    label.field input, label.field textarea, label.field select { font-size: 14px; }
    .providers-list { display: grid; gap: 16px; }
    .provider-card { position: relative; display: grid; gap: 14px; padding: 20px 20px 20px 24px; border: 1px solid #e2e8f0; border-radius: 12px; background: #fff; box-shadow: 0 1px 3px rgba(0,0,0,0.04); transition: all 0.2s ease; overflow: hidden; }
    .provider-card::before { content: ''; position: absolute; left: 0; top: 0; bottom: 0; width: 4px; background: linear-gradient(180deg, #2f6fed 0%, #6ea8fe 100%); border-radius: 12px 0 0 12px; }
    .provider-card:hover { border-color: #bdd4fd; box-shadow: 0 4px 16px rgba(47,111,237,0.1); transform: translateY(-1px); }
    .provider-card.disabled { opacity: 0.65; }
    .provider-card.disabled::before { background: linear-gradient(180deg, #bdc6d5 0%, #d9dee8 100%); }
    .provider-head { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
    .provider-icon { width: 42px; height: 42px; border-radius: 10px; background: linear-gradient(135deg, #eef4ff 0%, #dce9ff 100%); display: flex; align-items: center; justify-content: center; font-weight: 700; font-size: 17px; color: #2f6fed; flex-shrink: 0; }
    .provider-title { display: grid; gap: 2px; flex: 1; min-width: 0; }
    .provider-name { font-weight: 700; font-size: 16px; color: #172033; }
    .provider-id { color: #8896a8; font-size: 12px; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
    .provider-actions { display: flex; gap: 6px; flex-shrink: 0; }
    .provider-actions button { padding: 6px 14px; font-size: 12px; border-radius: 8px; font-weight: 500; }
    .provider-stats { display: flex; gap: 18px; align-items: center; color: #596579; font-size: 13px; }
    .provider-stats span { display: flex; align-items: center; gap: 6px; }
    .provider-stats .dot { width: 7px; height: 7px; border-radius: 50%; display: inline-block; flex-shrink: 0; }
    .provider-stats .dot.green { background: #22c55e; box-shadow: 0 0 0 2px rgba(34,197,94,0.15); }
    .provider-stats .dot.amber { background: #f59e0b; box-shadow: 0 0 0 2px rgba(245,158,11,0.15); }
    .provider-stats .dot.red { background: #ef4444; box-shadow: 0 0 0 2px rgba(239,68,68,0.15); }
    .provider-stats .dot.blue { background: #2f6fed; box-shadow: 0 0 0 2px rgba(47,111,237,0.15); }
    .provider-url { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; color: #8896a8; padding: 8px 12px; background: #f8f9fb; border-radius: 8px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; border: 1px solid #f0f2f5; }
    .badge { display: inline-flex; align-items: center; gap: 4px; border-radius: 999px; padding: 4px 10px; font-size: 11px; font-weight: 600; letter-spacing: 0.02em; background: #eef1f6; color: #39465a; }
    .badge.ok { background: #e7f6ed; color: #176342; }
    .badge.warn { background: #fff3d6; color: #8a5600; }
    .chips { display: flex; gap: 6px; flex-wrap: wrap; }
    .chip { border: 1px solid #e2e8f0; border-radius: 8px; padding: 4px 10px; font-size: 12px; background: #fafbfc; color: #475569; transition: all 0.15s ease; }
    .chip:hover { border-color: #bdd4fd; background: #eef4ff; }
    .chip.default { border-color: #2f6fed; background: #eef4ff; color: #2f6fed; font-weight: 600; }
    .providers-header { display: flex; align-items: center; justify-content: space-between; }
    .providers-subtitle { color: #8896a8; font-size: 13px; margin: 4px 0 0; }
    .providers-empty { text-align: center; padding: 56px 24px; color: #8896a8; }
    .providers-empty h3 { font-size: 15px; color: #596579; margin: 0 0 6px; font-weight: 600; }
    .providers-empty p { font-size: 13px; margin: 0; }
    .dialog-actions { display: flex; justify-content: flex-end; gap: 8px; }
    #agents-view { display: flex; flex-direction: column; padding: 0; overflow: hidden; }
    .agent-empty-state { display: flex; flex-direction: column; align-items: center; justify-content: center; flex: 1; color: #8896a8; gap: 8px; }
    .agent-empty-state h3 { font-size: 16px; color: #596579; margin: 0; font-weight: 600; }
    .agent-empty-state p { font-size: 13px; margin: 0; }
    .agent-workspace { display: flex; flex-direction: column; flex: 1; overflow: hidden; }
    .agent-workspace-header { display: flex; align-items: center; gap: 14px; padding: 16px 20px; background: #fff; border-bottom: 1px solid #eef1f6; flex-shrink: 0; }
    .agent-workspace-icon { width: 44px; height: 44px; border-radius: 12px; background: linear-gradient(135deg, #2f6fed 0%, #6ea8fe 100%); display: flex; align-items: center; justify-content: center; font-weight: 700; font-size: 18px; color: #fff; flex-shrink: 0; }
    .agent-workspace-info { flex: 1; min-width: 0; }
    .agent-workspace-info h2 { font-size: 18px; margin: 0; font-weight: 700; }
    .agent-workspace-meta { color: #8896a8; font-size: 13px; margin-top: 2px; display: flex; gap: 10px; align-items: center; flex-wrap: wrap; }
    .agent-workspace-meta .badge { margin: 0; }
    .agent-details-bar { display: flex; background: #fafbfc; border-bottom: 1px solid #eef1f6; overflow-x: auto; flex-shrink: 0; }
    .detail-cell { padding: 10px 20px; border-right: 1px solid #eef1f6; flex-shrink: 0; }
    .detail-cell:last-child { border-right: none; }
    .detail-cell-label { font-size: 10px; color: #8896a8; font-weight: 600; text-transform: uppercase; letter-spacing: 0.06em; margin-bottom: 3px; }
    .detail-cell-value { font-size: 13px; color: #172033; font-weight: 500; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
    .conversation { flex: 1; overflow-y: auto; padding: 20px; display: flex; flex-direction: column; gap: 16px; background: #f8f9fb; }
    .msg { display: flex; gap: 10px; max-width: 88%; animation: msgIn 0.2s ease; }
    @keyframes msgIn { from { opacity: 0; transform: translateY(6px); } to { opacity: 1; transform: translateY(0); } }
    .msg-user { flex-direction: row-reverse; align-self: flex-end; }
    .msg-avatar { width: 30px; height: 30px; border-radius: 8px; display: flex; align-items: center; justify-content: center; font-size: 11px; font-weight: 700; flex-shrink: 0; }
    .msg-user .msg-avatar { background: #2f6fed; color: #fff; }
    .msg-assistant .msg-avatar { background: #22c55e; color: #fff; }
    .msg-system .msg-avatar { background: #f59e0b; color: #fff; }
    .msg-tool .msg-avatar { background: #8b5cf6; color: #fff; }
    .msg-body { flex: 1; min-width: 0; }
    .msg-role { font-size: 11px; font-weight: 600; color: #8896a8; margin-bottom: 4px; text-transform: uppercase; letter-spacing: 0.05em; }
    .msg-user .msg-role { text-align: right; }
    .msg-content { padding: 12px 16px; border-radius: 12px; white-space: pre-wrap; overflow-wrap: anywhere; font-size: 14px; line-height: 1.6; }
    .msg-user .msg-content { background: #2f6fed; color: #fff; border-bottom-right-radius: 4px; }
    .msg-assistant .msg-content { background: #fff; border: 1px solid #e2e8f0; border-bottom-left-radius: 4px; box-shadow: 0 1px 2px rgba(0,0,0,0.04); }
    .msg-system .msg-content { background: #fffbeb; border: 1px solid #fde68a; color: #92400e; border-radius: 8px; font-size: 13px; }
    .msg-tool .msg-content { background: #f5f3ff; border: 1px solid #ddd6fe; color: #5b21b6; border-radius: 8px; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 13px; }
    .input-bar { display: flex; gap: 10px; align-items: flex-end; padding: 16px 20px; border-top: 1px solid #eef1f6; background: #fff; flex-shrink: 0; }
    .input-bar textarea { flex: 1; resize: none; border: 1px solid #e2e8f0; border-radius: 12px; padding: 10px 14px; font-size: 14px; min-height: 44px; max-height: 160px; line-height: 1.5; background: #fafbfc; transition: border-color 0.15s, box-shadow 0.15s; }
    .input-bar textarea:focus { outline: none; border-color: #2f6fed; background: #fff; box-shadow: 0 0 0 3px rgba(47,111,237,0.1); }
    .input-bar button { height: 44px; border-radius: 12px; padding: 0 24px; font-weight: 600; }
    .hidden { display: none !important; }
    pre { margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; }
    dialog { border: 1px solid #bdc6d5; border-radius: 10px; padding: 16px; width: min(640px, calc(100vw - 32px)); }
    dialog::backdrop { background: rgba(23, 32, 51, 0.32); }
    @media (max-width: 880px) { main { grid-template-columns: 1fr; } aside { border-right: 0; border-bottom: 1px solid #d9dee8; max-height: 36vh; } .form { grid-template-columns: 1fr; } }
  </style>
</head>
<body>
  <header>
    <h1>Mai Team</h1>
    <div class="tabs">
      <button class="tab active" data-tab="agents-view">Agents</button>
      <button class="tab" data-tab="providers-view">Providers</button>
    </div>
    <div class="toolbar">
      <button class="secondary" id="tokenButton">Token</button>
      <button id="refreshButton">Refresh</button>
    </div>
  </header>
  <main>
    <aside id="agents-sidebar">
      <div class="sidebar-header">
        <h2>Agents</h2>
        <button id="createAgentBtn">+ New</button>
      </div>
      <div id="agents" class="agent-list"></div>
    </aside>
    <section id="agents-view">
      <div id="agent-empty" class="agent-empty-state">
        <h3>No agent selected</h3>
        <p>Create a new agent or select one from the sidebar</p>
      </div>
      <div id="agent-workspace" class="agent-workspace hidden">
        <div class="agent-workspace-header" id="agentHeader"></div>
        <div class="agent-details-bar" id="agentDetails"></div>
        <div class="conversation" id="conversation"></div>
        <div class="input-bar">
          <textarea id="message" rows="1" placeholder="Send a command or message..."></textarea>
          <button id="send">Send</button>
        </div>
      </div>
    </section>
    <section class="grid hidden" id="providers-view">
      <div class="panel providers-header">
        <div>
          <h2>Providers</h2>
          <p class="providers-subtitle">Manage your LLM API providers and model configurations</p>
        </div>
        <button id="addProvider">+ Add Provider</button>
      </div>
      <div class="providers-list" id="providers"></div>
    </section>
  </main>
  <dialog id="tokenDialog">
    <form method="dialog" class="grid">
      <h2>Bearer Token</h2>
      <input id="tokenInput" placeholder="Paste token printed by server" />
      <div class="row">
        <button id="tokenSave" value="ok">Save</button>
      </div>
    </form>
  </dialog>
  <dialog id="providerDialog">
    <form method="dialog" class="grid">
      <h2 id="providerDialogTitle">Provider</h2>
      <div class="form">
        <label class="field">Provider ID
          <input id="providerId" placeholder="openai" />
        </label>
        <label class="field">Display Name
          <input id="providerName" placeholder="OpenAI" />
        </label>
        <label class="field full">OpenAI Base URL
          <input id="providerBaseUrl" placeholder="https://api.openai.com/v1" />
        </label>
        <label class="field">API Key
          <input id="providerApiKey" type="password" placeholder="Leave blank to keep existing key" />
        </label>
        <label class="field">Default Model
          <input id="providerDefaultModel" placeholder="gpt-5.2" />
        </label>
        <label class="field full">Models
          <textarea id="providerModels" rows="5" placeholder="One model per line"></textarea>
        </label>
        <label class="field">Enabled
          <select id="providerEnabled">
            <option value="true">Enabled</option>
            <option value="false">Disabled</option>
          </select>
        </label>
        <label class="field">Default Provider
          <select id="providerDefault">
            <option value="false">No</option>
            <option value="true">Yes</option>
          </select>
        </label>
      </div>
      <div class="dialog-actions">
        <button class="secondary" id="providerCancel" value="cancel">Cancel</button>
        <button id="providerSave" value="ok">Save</button>
      </div>
    </form>
  </dialog>
  <dialog id="agentDialog">
    <form method="dialog" class="grid">
      <h2>Create Agent</h2>
      <div class="form">
        <label class="field full">Agent Name
          <input id="agentNameInput" placeholder="My Agent" />
        </label>
        <label class="field">Provider
          <select id="agentProvider"></select>
        </label>
        <label class="field">Model
          <select id="agentModel"></select>
        </label>
      </div>
      <div class="dialog-actions">
        <button class="secondary" id="agentCreateCancel" value="cancel">Cancel</button>
        <button id="agentCreateSave" value="ok">Create</button>
      </div>
    </form>
  </dialog>
  <script>
    const tokenDialog = document.querySelector("#tokenDialog");
    const providerDialog = document.querySelector("#providerDialog");
    const tokenInput = document.querySelector("#tokenInput");
    const agentsEl = document.querySelector("#agents");
    const providersEl = document.querySelector("#providers");
    const providerSelect = document.querySelector("#agentProvider");
    const modelSelect = document.querySelector("#agentModel");
    const agentHeaderEl = document.querySelector("#agentHeader");
    const agentDetailsEl = document.querySelector("#agentDetails");
    const conversationEl = document.querySelector("#conversation");
    const agentEmptyEl = document.querySelector("#agent-empty");
    const agentWorkspaceEl = document.querySelector("#agent-workspace");
    let selected = null;
    let source = null;
    let providersState = { providers: [], default_provider_id: null };
    let editingProviderIndex = null;
    let token = localStorage.getItem("maiToken") || new URLSearchParams(location.search).get("token") || "";

    document.querySelectorAll(".tab").forEach(button => button.onclick = () => switchTab(button.dataset.tab));
    document.querySelector("#tokenButton").onclick = () => promptToken("Enter bearer token");
    document.querySelector("#refreshButton").onclick = () => refreshAll();
    document.querySelector("#createAgentBtn").onclick = () => { document.querySelector("#agentNameInput").value = ""; renderProviderSelect(); document.querySelector("#agentDialog").showModal(); };
    document.querySelector("#agentCreateSave").onclick = async (e) => { e.preventDefault(); const body = { name: document.querySelector("#agentNameInput").value || null, provider_id: providerSelect.value || null, model: modelSelect.value || null }; const created = await api("/agents", { method: "POST", body: JSON.stringify(body) }); selected = created.agent.id; document.querySelector("#agentDialog").close(); await refreshAgents(); await refreshDetail(); };
    document.querySelector("#send").onclick = sendMessage;
    document.querySelector("#message").addEventListener("input", function() { this.style.height = "auto"; this.style.height = Math.min(this.scrollHeight, 160) + "px"; });
    document.querySelector("#message").addEventListener("keydown", function(e) { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendMessage(); } });
    document.querySelector("#addProvider").onclick = () => openProviderDialog(null);
    document.querySelector("#providerSave").onclick = saveProviderDialog;
    document.querySelector("#tokenSave").onclick = () => {
      token = tokenInput.value.trim();
      localStorage.setItem("maiToken", token);
      connectEvents();
      refreshAll();
    };
    providerSelect.onchange = renderModelSelect;

    async function api(path, init = {}, retry = true) {
      if (!token) await promptToken("Enter bearer token");
      const headers = { "authorization": `Bearer ${token}`, "content-type": "application/json", ...(init.headers || {}) };
      const res = await fetch(path, { ...init, headers });
      if (res.status === 401 && retry) {
        await promptToken("Token expired or invalid");
        return api(path, init, false);
      }
      if (!res.ok) throw new Error(await res.text());
      if (res.status === 204) return null;
      return res.json();
    }

    function promptToken(title) {
      return new Promise(resolve => {
        tokenInput.value = token;
        tokenDialog.querySelector("h2").textContent = title;
        tokenDialog.onclose = () => resolve();
        tokenDialog.showModal();
      });
    }

    async function refreshAll() {
      await Promise.all([loadProviders(), refreshAgents()]);
      if (selected) await refreshDetail();
    }

    async function loadProviders() {
      providersState = await api("/providers");
      renderProviderSelect();
      renderProviders();
    }

    function renderProviderSelect() {
      providerSelect.innerHTML = providersState.providers.map(p => `<option value="${escapeHtml(p.id)}">${escapeHtml(p.name)}</option>`).join("");
      providerSelect.value = providersState.default_provider_id || providersState.providers[0]?.id || "";
      renderModelSelect();
    }

    function renderModelSelect() {
      const provider = providersState.providers.find(p => p.id === providerSelect.value);
      const models = provider?.models || [];
      modelSelect.innerHTML = models.map(m => `<option value="${escapeHtml(m)}">${escapeHtml(m)}</option>`).join("");
      if (provider) modelSelect.value = provider.default_model;
    }

    async function sendMessage() {
      if (!selected) return;
      const msgInput = document.querySelector("#message");
      const message = msgInput.value;
      if (!message.trim()) return;
      await api(`/agents/${selected}/messages`, { method: "POST", body: JSON.stringify({ message }) });
      msgInput.value = "";
      msgInput.style.height = "auto";
      refreshDetail();
    }

    function statusDotClass(status) {
      const s = String(status || "").toLowerCase();
      if (s.includes("run") || s.includes("turn") || s.includes("start") || s.includes("wait")) return "running";
      if (s.includes("fail") || s.includes("error") || s.includes("cancel")) return "error";
      if (s.includes("creat") || s.includes("delet")) return "pending";
      if (s.includes("complet")) return "done";
      return "idle";
    }

    function formatStatus(status) {
      return String(status || "unknown").replace(/_/g, " ").replace(/\b\w/g, c => c.toUpperCase());
    }

    async function refreshAgents() {
      const agents = await api("/agents");
      if (!agents.length) {
        agentsEl.innerHTML = `<div class="agent-empty-sidebar">No agents yet</div>`;
        return;
      }
      agentsEl.innerHTML = agents.map(a => {
        const initial = (a.name || "A").charAt(0).toUpperCase();
        const dotClass = statusDotClass(a.status);
        return `<div class="agent-item ${a.id === selected ? "active" : ""}" data-id="${a.id}">
          <div class="agent-avatar">${initial}</div>
          <div class="agent-info">
            <div class="name">${escapeHtml(a.name)}</div>
            <div class="meta">${escapeHtml(a.provider_name)} / ${escapeHtml(a.model)}</div>
          </div>
          <span class="status-dot ${dotClass}"></span>
        </div>`;
      }).join("");
      agentsEl.querySelectorAll(".agent-item").forEach(el => el.onclick = () => { selected = el.dataset.id; refreshAgents(); refreshDetail(); });
    }

    async function refreshDetail() {
      if (!selected) {
        agentEmptyEl.classList.remove("hidden");
        agentWorkspaceEl.classList.add("hidden");
        return;
      }
      const detail = await api(`/agents/${selected}`);
      const s = detail.summary || detail;
      agentEmptyEl.classList.add("hidden");
      agentWorkspaceEl.classList.remove("hidden");
      renderAgentHeader(s);
      renderAgentDetails(s);
      renderConversation(detail.messages || []);
    }

    function renderAgentHeader(s) {
      const initial = (s.name || "A").charAt(0).toUpperCase();
      const dotClass = statusDotClass(s.status);
      const statusLabel = formatStatus(s.status);
      agentHeaderEl.innerHTML = `
        <div class="agent-workspace-icon">${initial}</div>
        <div class="agent-workspace-info">
          <h2>${escapeHtml(s.name)}</h2>
          <div class="agent-workspace-meta">
            <span class="badge ${dotClass === "running" || dotClass === "idle" ? "ok" : dotClass === "error" ? "warn" : "ok"}">${statusLabel}</span>
            <span>${escapeHtml(s.provider_name)} / ${escapeHtml(s.model)}</span>
            ${s.last_error ? `<span style="color:#b42318;font-size:12px">${escapeHtml(s.last_error)}</span>` : ""}
          </div>
        </div>`;
    }

    function renderAgentDetails(s) {
      const container = s.container_id ? s.container_id.substring(0, 12) : "none";
      const tokens = s.token_usage ? `${s.token_usage.total_tokens.toLocaleString()}` : "0";
      const created = s.created_at ? new Date(s.created_at).toLocaleString() : "-";
      agentDetailsEl.innerHTML = `
        <div class="detail-cell"><div class="detail-cell-label">Status</div><div class="detail-cell-value">${formatStatus(s.status)}</div></div>
        <div class="detail-cell"><div class="detail-cell-label">Container</div><div class="detail-cell-value">${escapeHtml(container)}</div></div>
        <div class="detail-cell"><div class="detail-cell-label">Provider</div><div class="detail-cell-value">${escapeHtml(s.provider_id || "-")}</div></div>
        <div class="detail-cell"><div class="detail-cell-label">Model</div><div class="detail-cell-value">${escapeHtml(s.model || "-")}</div></div>
        <div class="detail-cell"><div class="detail-cell-label">Tokens</div><div class="detail-cell-value">${tokens}</div></div>
        <div class="detail-cell"><div class="detail-cell-label">Created</div><div class="detail-cell-value">${created}</div></div>`;
    }

    function renderConversation(messages) {
      conversationEl.innerHTML = messages.map(m => {
        const role = String(m.role || "").toLowerCase();
        const roleLabel = { user: "You", assistant: "Assistant", system: "System", tool: "Tool" }[role] || role;
        const initial = { user: "U", assistant: "A", system: "S", tool: "T" }[role] || "?";
        return `<div class="msg msg-${role}">
          <div class="msg-avatar">${initial}</div>
          <div class="msg-body">
            <div class="msg-role">${roleLabel}</div>
            <div class="msg-content">${escapeHtml(m.content)}</div>
          </div>
        </div>`;
      }).join("");
      conversationEl.scrollTop = conversationEl.scrollHeight;
    }

    function renderProviders() {
      if (!providersState.providers.length) {
        providersEl.innerHTML = `<div class="providers-empty"><h3>No providers yet</h3><p>Add an LLM provider to get started with your agents.</p></div>`;
        return;
      }
      providersEl.innerHTML = providersState.providers.map((provider, index) => {
        const isDefault = providersState.default_provider_id === provider.id;
        const models = provider.models || [];
        const initial = (provider.name || "P").charAt(0).toUpperCase();
        return `<div class="provider-card ${provider.enabled ? "" : "disabled"}" data-index="${index}">
        <div class="provider-head">
          <div class="provider-icon">${initial}</div>
          <div class="provider-title">
            <div class="provider-name">${escapeHtml(provider.name || "Provider")}</div>
            <div class="provider-id">${escapeHtml(provider.id)}</div>
          </div>
          <div class="provider-actions">
            <button class="secondary" data-edit="${index}">Edit</button>
            <button class="danger" data-remove="${index}">Delete</button>
          </div>
        </div>
        <div class="provider-stats">
          <span><span class="dot ${provider.enabled ? "green" : "amber"}"></span>${provider.enabled ? "Enabled" : "Disabled"}</span>
          ${isDefault ? `<span><span class="dot blue"></span>Default</span>` : ""}
          <span><span class="dot ${provider.has_api_key ? "green" : "red"}"></span>${provider.has_api_key ? "API key set" : "No API key"}</span>
          <span>${models.length} model${models.length !== 1 ? "s" : ""}</span>
        </div>
        <div class="provider-url">${escapeHtml(provider.base_url)}</div>
        <div class="chips">${models.map(model => `<span class="chip ${model === provider.default_model ? "default" : ""}">${escapeHtml(model)}</span>`).join("") || `<span class="chip">No models</span>`}</div>
      </div>`;
      }).join("");
      providersEl.querySelectorAll("[data-edit]").forEach(button => button.onclick = () => openProviderDialog(Number(button.dataset.edit)));
      providersEl.querySelectorAll("[data-remove]").forEach(button => button.onclick = () => {
        providersState.providers.splice(Number(button.dataset.remove), 1);
        if (!providersState.providers.some(provider => provider.id === providersState.default_provider_id)) {
          providersState.default_provider_id = providersState.providers[0]?.id || null;
        }
        saveProviders();
      });
    }

    function openProviderDialog(index) {
      editingProviderIndex = index;
      const provider = index === null ? {
        id: `provider-${providersState.providers.length + 1}`,
        name: "New Provider",
        base_url: "https://api.openai.com/v1",
        models: ["gpt-5.2"],
        default_model: "gpt-5.2",
        enabled: true,
        has_api_key: false
      } : providersState.providers[index];
      document.querySelector("#providerDialogTitle").textContent = index === null ? "Add Provider" : "Edit Provider";
      document.querySelector("#providerId").value = provider.id || "";
      document.querySelector("#providerName").value = provider.name || "";
      document.querySelector("#providerBaseUrl").value = provider.base_url || "";
      document.querySelector("#providerApiKey").value = "";
      document.querySelector("#providerApiKey").placeholder = provider.has_api_key ? "Leave blank to keep existing key" : "API key";
      document.querySelector("#providerDefaultModel").value = provider.default_model || "";
      document.querySelector("#providerModels").value = (provider.models || []).join("\n");
      document.querySelector("#providerEnabled").value = provider.enabled === false ? "false" : "true";
      document.querySelector("#providerDefault").value = providersState.default_provider_id === provider.id ? "true" : "false";
      providerDialog.showModal();
    }

    async function saveProviderDialog(event) {
      event.preventDefault();
      const provider = {
        id: document.querySelector("#providerId").value.trim(),
        name: document.querySelector("#providerName").value.trim(),
        base_url: document.querySelector("#providerBaseUrl").value.trim(),
        api_key: document.querySelector("#providerApiKey").value,
        default_model: document.querySelector("#providerDefaultModel").value.trim(),
        models: document.querySelector("#providerModels").value.split(/\n|,/).map(v => v.trim()).filter(Boolean),
        enabled: document.querySelector("#providerEnabled").value === "true"
      };
      if (!provider.id || !provider.name || !provider.base_url || !provider.default_model) {
        alert("Provider ID, Display Name, Base URL, and Default Model are required.");
        return;
      }
      if (editingProviderIndex === null) {
        providersState.providers.push(provider);
      } else {
        const existing = providersState.providers[editingProviderIndex];
        providersState.providers[editingProviderIndex] = { ...existing, ...provider, has_api_key: provider.api_key ? true : existing.has_api_key };
      }
      if (document.querySelector("#providerDefault").value === "true" || !providersState.default_provider_id) {
        providersState.default_provider_id = provider.id;
      } else if (!providersState.providers.some(item => item.id === providersState.default_provider_id)) {
        providersState.default_provider_id = providersState.providers[0]?.id || null;
      }
      await saveProviders();
      providerDialog.close();
    }

    async function saveProviders() {
      providersState = await api("/providers", {
        method: "PUT",
        body: JSON.stringify({ providers: providersState.providers, default_provider_id: providersState.default_provider_id || providersState.providers[0]?.id || null })
      });
      renderProviderSelect();
      renderProviders();
    }

    function connectEvents() {
      if (source) source.close();
      if (!token) return;
      source = new EventSource(`/events?token=${encodeURIComponent(token)}`);
      source.onmessage = event => appendEvent(event.data);
      source.onerror = async () => {
        source.close();
        await promptToken("Token expired or invalid");
        connectEvents();
      };
      ["agent_created","agent_status_changed","turn_started","turn_completed","tool_started","tool_completed","agent_message","error"].forEach(name => {
        source.addEventListener(name, event => { appendEvent(event.data); refreshAgents(); if (selected) refreshDetail(); });
      });
    }

    function switchTab(id) {
      document.querySelectorAll(".tab").forEach(tab => tab.classList.toggle("active", tab.dataset.tab === id));
      document.querySelector("#agents-view").classList.toggle("hidden", id !== "agents-view");
      document.querySelector("#providers-view").classList.toggle("hidden", id !== "providers-view");
    }

    function appendEvent(data) {
      console.log("[event]", data);
    }

    function escapeHtml(value) {
      return String(value ?? "").replace(/[&<>"']/g, ch => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[ch]));
    }

    connectEvents();
    refreshAll().catch(err => appendEvent(err.message));
  </script>
</body>
</html>
"##;
