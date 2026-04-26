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
use mai_model::{ModelConfig, ResponsesClient};
use mai_protocol::{
    AgentId, CreateAgentRequest, CreateAgentResponse, ErrorResponse, FileUploadRequest,
    FileUploadResponse, SendMessageRequest, SendMessageResponse, ServiceEvent,
};
use mai_runtime::{AgentRuntime, RuntimeConfig, RuntimeError};
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

#[derive(Clone)]
struct AppState {
    runtime: Arc<AgentRuntime>,
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

    let token = env::var("MAI_TEAM_TOKEN").context("MAI_TEAM_TOKEN must be set")?;
    let api_key = env::var("OPENAI_API_KEY").context("OPENAI_API_KEY must be set")?;
    let base_url =
        env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-5.2".to_string());
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

    let model = ResponsesClient::new(ModelConfig {
        api_key,
        base_url,
        model,
    });
    let runtime = AgentRuntime::new(
        docker,
        model,
        RuntimeConfig {
            repo_root: env::current_dir()?,
        },
    )?;
    let state = Arc::new(AppState { runtime, token });

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
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

const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Mai Team</title>
  <style>
    :root { color-scheme: light dark; font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
    body { margin: 0; background: #f6f7f9; color: #172033; }
    header { display: flex; align-items: center; gap: 12px; padding: 16px 20px; background: #ffffff; border-bottom: 1px solid #d9dee8; }
    h1 { font-size: 18px; margin: 0; }
    main { display: grid; grid-template-columns: 360px 1fr; min-height: calc(100vh - 58px); }
    aside { border-right: 1px solid #d9dee8; background: #ffffff; overflow: auto; }
    section { padding: 16px; overflow: auto; }
    input, button, textarea { font: inherit; }
    input, textarea { border: 1px solid #bdc6d5; border-radius: 6px; padding: 8px; background: #fff; color: #172033; }
    button { border: 1px solid #2f6fed; border-radius: 6px; padding: 8px 10px; background: #2f6fed; color: white; cursor: pointer; }
    .toolbar { display: flex; gap: 8px; align-items: center; margin-left: auto; }
    .token { width: 260px; }
    .agent { padding: 12px 16px; border-bottom: 1px solid #eef1f6; cursor: pointer; }
    .agent:hover { background: #eef4ff; }
    .agent.active { background: #dce9ff; }
    .name { font-weight: 650; }
    .meta { color: #596579; font-size: 12px; margin-top: 4px; overflow-wrap: anywhere; }
    .grid { display: grid; gap: 12px; }
    .panel { background: #fff; border: 1px solid #d9dee8; border-radius: 8px; padding: 12px; }
    .row { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
    .messages { display: grid; gap: 8px; }
    .message { border-left: 3px solid #bdc6d5; padding: 8px 10px; background: #fafbfc; white-space: pre-wrap; overflow-wrap: anywhere; }
    .message.assistant { border-left-color: #2f9e6d; }
    .message.user { border-left-color: #2f6fed; }
    pre { margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; }
    @media (max-width: 820px) { main { grid-template-columns: 1fr; } aside { border-right: 0; border-bottom: 1px solid #d9dee8; max-height: 42vh; } }
  </style>
</head>
<body>
  <header>
    <h1>Mai Team</h1>
    <div class="toolbar">
      <input class="token" id="token" placeholder="Bearer token" />
      <button id="save">Save</button>
      <button id="create">New Agent</button>
    </div>
  </header>
  <main>
    <aside id="agents"></aside>
    <section class="grid">
      <div class="panel">
        <div class="row">
          <textarea id="message" rows="3" style="flex:1" placeholder="Send a task"></textarea>
          <button id="send">Send</button>
        </div>
      </div>
      <div class="panel">
        <pre id="detail">No agent selected.</pre>
      </div>
      <div class="panel messages" id="messages"></div>
      <div class="panel">
        <pre id="events"></pre>
      </div>
    </section>
  </main>
  <script>
    const tokenInput = document.querySelector("#token");
    const agentsEl = document.querySelector("#agents");
    const detailEl = document.querySelector("#detail");
    const messagesEl = document.querySelector("#messages");
    const eventsEl = document.querySelector("#events");
    let selected = null;
    let source = null;
    tokenInput.value = localStorage.getItem("maiToken") || new URLSearchParams(location.search).get("token") || "";
    document.querySelector("#save").onclick = () => { localStorage.setItem("maiToken", tokenInput.value); connectEvents(); refresh(); };
    document.querySelector("#create").onclick = async () => { await api("/agents", { method: "POST", body: JSON.stringify({}) }); refresh(); };
    document.querySelector("#send").onclick = async () => {
      if (!selected) return;
      const message = document.querySelector("#message").value;
      await api(`/agents/${selected}/messages`, { method: "POST", body: JSON.stringify({ message }) });
      document.querySelector("#message").value = "";
      refreshDetail();
    };
    async function api(path, init = {}) {
      const headers = { "authorization": `Bearer ${tokenInput.value}`, "content-type": "application/json", ...(init.headers || {}) };
      const res = await fetch(path, { ...init, headers });
      if (!res.ok) throw new Error(await res.text());
      if (res.status === 204) return null;
      return res.json();
    }
    async function refresh() {
      if (!tokenInput.value) return;
      const agents = await api("/agents");
      agentsEl.innerHTML = agents.map(a => `<div class="agent ${a.id === selected ? "active" : ""}" data-id="${a.id}">
        <div class="name">${escapeHtml(a.name)}</div>
        <div class="meta">${a.status} · ${a.container_id || "no container"}</div>
      </div>`).join("");
      agentsEl.querySelectorAll(".agent").forEach(el => el.onclick = () => { selected = el.dataset.id; refresh(); refreshDetail(); });
    }
    async function refreshDetail() {
      if (!selected) return;
      const detail = await api(`/agents/${selected}`);
      detailEl.textContent = JSON.stringify(detail.summary, null, 2);
      messagesEl.innerHTML = detail.messages.map(m => `<div class="message ${m.role}"><b>${m.role}</b>\n${escapeHtml(m.content)}</div>`).join("");
    }
    function connectEvents() {
      if (source) source.close();
      if (!tokenInput.value) return;
      source = new EventSource(`/events?token=${encodeURIComponent(tokenInput.value)}`);
      source.onmessage = event => appendEvent(event.data);
      ["agent_created","agent_status_changed","turn_started","turn_completed","tool_started","tool_completed","agent_message","error"].forEach(name => {
        source.addEventListener(name, event => { appendEvent(event.data); refresh(); if (selected) refreshDetail(); });
      });
    }
    function appendEvent(data) {
      eventsEl.textContent = `${data}\n${eventsEl.textContent}`.slice(0, 12000);
    }
    function escapeHtml(value) {
      return value.replace(/[&<>"']/g, ch => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[ch]));
    }
    connectEvents();
    refresh();
  </script>
</body>
</html>
"##;
