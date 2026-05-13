use anyhow::{Context, Result};
mod relay;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::http::{StatusCode, Uri, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use clap::Parser;
use futures::StreamExt;
use mai_docker::DockerClient;
use mai_model::{
    ModelClient, ModelError, ModelStreamAccumulator, ModelTurnState, ResolvedProvider,
};
use mai_protocol::{
    AgentConfigRequest, AgentConfigResponse, AgentId, AgentLogsResponse, AgentProfilesResponse,
    ApproveTaskPlanResponse, ArtifactInfo, CreateAgentRequest, CreateAgentResponse,
    CreateProjectRequest, CreateProjectResponse, CreateSessionResponse, CreateTaskRequest,
    CreateTaskResponse, ErrorResponse, FileUploadRequest, FileUploadResponse,
    GitAccountDefaultRequest, GitAccountRequest, GitAccountResponse, GitAccountsResponse,
    GithubAppInstallationPackagesRequest, GithubAppInstallationStartRequest,
    GithubAppInstallationStartResponse, GithubAppManifestStartRequest,
    GithubAppManifestStartResponse, GithubAppSettingsRequest, GithubAppSettingsResponse,
    GithubInstallationsResponse, GithubRepositoriesResponse, GithubSettingsRequest,
    GithubSettingsResponse, McpServersConfigRequest, ModelInputItem, ModelOutputItem,
    ModelResponse, ProjectId, ProjectReviewRunDetail, ProjectReviewRunsResponse,
    ProviderPresetsResponse, ProviderTestRequest, ProviderTestResponse, ProvidersConfigRequest,
    ProvidersResponse, RelayStatusResponse, RepositoryPackagesResponse, RequestPlanRevisionRequest,
    RequestPlanRevisionResponse, RuntimeDefaultsResponse, SendMessageRequest, SendMessageResponse,
    ServiceEvent, SessionId, SkillsConfigRequest, SkillsListResponse, TaskId, ToolTraceDetail,
    ToolTraceListResponse, TurnId, UpdateAgentRequest, UpdateAgentResponse, UpdateProjectRequest,
    UpdateProjectResponse,
};
use mai_runtime::{AgentRuntime, RuntimeConfig, RuntimeError};
use mai_store::{AgentLogFilter, ConfigStore, ToolTraceFilter};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::env;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;
use tokio_stream::once;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

const SSE_REPLAY_LIMIT: usize = 1_000;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[arg(long = "data-path", value_name = "PATH")]
    data_path: Option<PathBuf>,
}

#[derive(Clone)]
struct AppState {
    runtime: Arc<AgentRuntime>,
    store: Arc<ConfigStore>,
    relay: Option<Arc<relay::RelayClient>>,
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    last_event_id: Option<u64>,
}

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/static"]
struct StaticAssets;

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/system-skills"]
struct EmbeddedSystemSkills;

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/system-agents"]
struct EmbeddedSystemAgents;

static EMBEDDED_RESOURCE_RELEASE_LOCK: StdMutex<()> = StdMutex::new(());

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl From<RuntimeError> for ApiError {
    fn from(value: RuntimeError) -> Self {
        let status = match value {
            RuntimeError::AgentNotFound(_)
            | RuntimeError::TaskNotFound(_)
            | RuntimeError::ProjectNotFound(_)
            | RuntimeError::ProjectReviewRunNotFound(_) => StatusCode::NOT_FOUND,
            RuntimeError::TurnNotFound { .. } => StatusCode::NOT_FOUND,
            RuntimeError::SessionNotFound { .. } => StatusCode::NOT_FOUND,
            RuntimeError::ToolTraceNotFound { .. } => StatusCode::NOT_FOUND,
            RuntimeError::AgentBusy(_) | RuntimeError::TaskBusy(_) => StatusCode::CONFLICT,
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

impl ApiError {
    fn bad_request(message: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message,
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

#[derive(Debug, Deserialize)]
struct AgentDetailQuery {
    session_id: Option<SessionId>,
}

#[derive(Debug, Deserialize)]
struct AgentLogsQuery {
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
    level: Option<String>,
    category: Option<String>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ToolTraceListQuery {
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct TaskDetailQuery {
    agent_id: Option<AgentId>,
}

#[derive(Debug, Deserialize)]
struct ProjectDetailQuery {
    agent_id: Option<AgentId>,
    session_id: Option<SessionId>,
}

#[derive(Debug, Deserialize)]
struct ProjectReviewRunsQuery {
    offset: Option<usize>,
    limit: Option<usize>,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mai_server=info,mai_runtime=info,tower_http=info".into()),
        )
        .init();

    let api_key = env::var("OPENAI_API_KEY").ok();
    let base_url =
        env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-5.5".to_string());
    let data_dir = data_dir_path(cli.data_path)?;
    let cache_dir = cache_dir_path(&data_dir);
    let artifact_files_root = artifact_files_root(&data_dir);
    let artifact_index_root = artifact_index_root(&data_dir);
    let image = env::var("MAI_AGENT_BASE_IMAGE")
        .unwrap_or_else(|_| "ghcr.io/zr233/mai-team-agent:latest".to_string());
    let sidecar_image = env::var("MAI_SIDECAR_IMAGE")
        .unwrap_or_else(|_| "ghcr.io/zr233/mai-team-sidecar:latest".to_string());
    let bind = env::var("MAI_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let addr: SocketAddr = bind.parse().context("invalid MAI_BIND_ADDR")?;

    let docker = DockerClient::new(image);
    let docker_version = docker.check_available().await?;
    info!("docker available: {docker_version}");

    fs::create_dir_all(&cache_dir)?;
    fs::create_dir_all(&artifact_files_root)?;
    fs::create_dir_all(&artifact_index_root)?;

    let store = Arc::new(ConfigStore::open_in_data_dir(&data_dir).await?);
    store
        .seed_default_provider_from_env(api_key, base_url, model)
        .await?;

    let system_skills_root = system_skills_path(&data_dir);
    release_embedded_system_skills(&system_skills_root)?;
    info!(
        path = %system_skills_root.display(),
        "released embedded system skills"
    );
    let system_agents_root = system_agents_path(&data_dir);
    release_embedded_system_agents(&system_agents_root)?;
    info!(
        path = %system_agents_root.display(),
        "released embedded system agents"
    );

    let model = ModelClient::new();
    let runtime_config = RuntimeConfig {
        repo_root: env::current_dir()?,
        cache_root: cache_dir.clone(),
        artifact_files_root: artifact_files_root.clone(),
        sidecar_image,
        github_api_base_url: None,
        git_binary: None,
        system_skills_root: Some(system_skills_root),
        system_agents_root: Some(system_agents_root),
    };
    info!(
        data_dir = %data_dir.display(),
        cache_dir = %cache_dir.display(),
        artifact_files_root = %artifact_files_root.display(),
        artifact_index_root = %artifact_index_root.display(),
        "runtime storage paths"
    );
    let relay = relay_config_from_env().map(|config| Arc::new(relay::RelayClient::new(config)));
    let github_backend = relay
        .as_ref()
        .map(|client| Arc::clone(client) as Arc<dyn mai_runtime::GithubAppBackend>);
    let runtime = AgentRuntime::new_with_github_backend(
        docker,
        model,
        Arc::clone(&store),
        runtime_config,
        github_backend,
    )
    .await?;
    if let Some(relay) = &relay {
        relay.set_runtime(Arc::clone(&runtime)).await;
        Arc::clone(relay).start();
    }
    let cleaned = runtime.cleanup_orphaned_containers().await?;
    if !cleaned.is_empty() {
        info!(
            count = cleaned.len(),
            "removed orphaned mai-team containers"
        );
    }
    let state = Arc::new(AppState {
        runtime,
        store,
        relay,
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/providers", get(get_providers).put(save_providers))
        .route("/providers/{id}/test", post(test_provider))
        .route("/mcp-servers", get(get_mcp_servers).put(save_mcp_servers))
        .route(
            "/git/accounts",
            get(list_git_accounts).post(save_git_account),
        )
        .route(
            "/git/accounts/default",
            axum::routing::put(set_default_git_account),
        )
        .route(
            "/git/accounts/{id}",
            axum::routing::put(save_git_account_by_id).delete(delete_git_account),
        )
        .route("/git/accounts/{id}/verify", post(verify_git_account))
        .route(
            "/git/accounts/{id}/repositories",
            get(list_git_account_repositories),
        )
        .route(
            "/git/accounts/{id}/repositories/{owner}/{repo}/packages",
            get(list_git_account_repository_packages),
        )
        .route("/runtime/defaults", get(get_runtime_defaults))
        .route(
            "/settings/github",
            get(get_github_settings).put(save_github_settings),
        )
        .route(
            "/settings/github-app",
            get(get_github_app_settings).put(save_github_app_settings),
        )
        .route(
            "/github/app-manifest/start",
            post(start_github_app_manifest),
        )
        .route(
            "/github/app-manifest/callback",
            get(complete_github_app_manifest),
        )
        .route(
            "/github/app-installation/callback",
            get(github_app_installation_callback),
        )
        .route(
            "/github/app-installation/start",
            post(start_github_app_installation),
        )
        .route("/relay/status", get(get_relay_status))
        .route("/github/installations", get(list_github_installations))
        .route(
            "/github/installations:refresh",
            post(refresh_github_installations),
        )
        .route(
            "/github/installations/{id}/repositories",
            get(list_github_repositories),
        )
        .route(
            "/github/installations/{id}/repositories/{owner}/{repo}/packages",
            get(list_github_repository_packages),
        )
        .route("/provider-presets", get(get_provider_presets))
        .route("/skills", get(list_skills))
        .route("/skills/config", axum::routing::put(save_skills_config))
        .route("/agent-profiles", get(list_agent_profiles))
        .route("/agent-profiles:reload", post(list_agent_profiles))
        .route(
            "/agent-config",
            get(get_agent_config).put(save_agent_config),
        )
        .route("/events", get(events))
        .route("/tasks", get(list_tasks).post(create_task))
        .route("/tasks:ensure-default", post(ensure_default_task))
        .route("/tasks/{id}", get(get_task).delete(delete_task))
        .route("/tasks/{id}/messages", post(send_task_message))
        .route("/tasks/{id}/plan:approve", post(approve_task_plan))
        .route(
            "/tasks/{id}/plan:request-revision",
            post(request_plan_revision),
        )
        .route("/tasks/{id}/cancel", post(cancel_task))
        .route("/projects", get(list_projects).post(create_project))
        .route(
            "/projects/{id}",
            get(get_project)
                .patch(update_project)
                .delete(delete_project),
        )
        .route("/projects/{id}/messages", post(send_project_message))
        .route("/projects/{id}/review-runs", get(list_project_review_runs))
        .route(
            "/projects/{id}/review-runs/{run_id}",
            get(get_project_review_run),
        )
        .route("/projects/{id}/skills", get(list_project_skills))
        .route("/projects/{id}/skills/detect", post(detect_project_skills))
        .route("/projects/{id}/cancel", post(cancel_project))
        .route("/agents", get(list_agents).post(create_agent))
        .route(
            "/agents/{id}",
            get(get_agent)
                .delete(delete_agent)
                .patch(update_agent)
                .post(cancel_agent_colon),
        )
        .route("/agents/{id}/messages", post(send_message))
        .route("/agents/{id}/sessions", post(create_session))
        .route(
            "/agents/{id}/sessions/{session_id}/messages",
            post(send_session_message),
        )
        .route(
            "/agents/{id}/sessions/{session_id}/tool-calls/{call_id}",
            get(get_session_tool_trace),
        )
        .route("/agents/{id}/logs", get(list_agent_logs))
        .route("/agents/{id}/tool-calls", get(list_tool_traces))
        .route("/agents/{id}/tool-calls/{call_id}", get(get_tool_trace))
        .route(
            "/agents/{id}/turns/{turn_id}/cancel",
            post(cancel_agent_turn),
        )
        .route("/agents/{id}/files:upload", post(upload_file))
        .route("/agents/{id}/files:download", get(download_file))
        .route("/tasks/{id}/artifacts", get(list_artifacts))
        .route("/artifacts/{id}/download", get(download_artifact))
        .route("/agents/{id}/cancel", post(cancel_agent))
        .fallback(get(static_fallback))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    println!("Open http://{addr}/");
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

fn system_skills_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("system-skills")
}

fn system_agents_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("system-agents")
}

fn data_dir_path(cli_data_path: Option<PathBuf>) -> Result<PathBuf> {
    Ok(match cli_data_path {
        Some(path) => path,
        None => env::current_dir()?.join(".mai-team"),
    })
}

#[cfg(test)]
fn data_dir_path_with(current_dir: &std::path::Path, cli_data_path: Option<PathBuf>) -> PathBuf {
    cli_data_path.unwrap_or_else(|| current_dir.join(".mai-team"))
}

fn cache_dir_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("cache")
}

fn artifact_files_root(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("artifacts").join("files")
}

fn artifact_index_root(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("artifacts").join("index")
}

fn relay_config_from_env() -> Option<relay::RelayClientConfig> {
    let enabled = env::var("MAI_RELAY_ENABLED")
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "on"));
    if !enabled {
        return None;
    }
    let token = env::var("MAI_RELAY_TOKEN").unwrap_or_default();
    if token.trim().is_empty() {
        tracing::warn!("MAI_RELAY_ENABLED is set but MAI_RELAY_TOKEN is empty; relay disabled");
        return None;
    }
    let node_id = env::var("MAI_RELAY_NODE_ID").unwrap_or_else(|_| "mai-server".to_string());
    Some(relay::RelayClientConfig {
        url: relay_url_from_env_values(
            env::var("MAI_RELAY_PUBLIC_URL").ok().as_deref(),
            env::var("MAI_RELAY_URL").ok().as_deref(),
        ),
        token,
        node_id,
    })
}

fn relay_url_from_env_values(public_url: Option<&str>, legacy_url: Option<&str>) -> String {
    public_url
        .or(legacy_url)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("http://127.0.0.1:8090")
        .trim_end_matches('/')
        .to_string()
}

fn release_embedded_system_skills(target_dir: &std::path::Path) -> io::Result<()> {
    release_embedded_resources::<EmbeddedSystemSkills>(
        target_dir,
        safe_system_resource_target,
        "system-skills",
    )
}

fn release_embedded_system_agents(target_dir: &std::path::Path) -> io::Result<()> {
    release_embedded_resources::<EmbeddedSystemAgents>(
        target_dir,
        safe_system_resource_target,
        "system-agents",
    )
}

fn release_embedded_resources<E>(
    target_dir: &std::path::Path,
    is_safe_target: fn(&std::path::Path) -> bool,
    out_dir_name: &str,
) -> io::Result<()>
where
    E: RustEmbed,
{
    let _guard = EMBEDDED_RESOURCE_RELEASE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !is_safe_target(target_dir) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsafe system resource target: {}", target_dir.display()),
        ));
    }
    if target_dir.exists() {
        fs::remove_dir_all(target_dir)?;
    }
    fs::create_dir_all(target_dir)?;
    for path in E::iter() {
        let path = path.as_ref();
        let Some(relative) = embedded_system_resource_relative_path(path, out_dir_name) else {
            continue;
        };
        let target = target_dir.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(asset) = E::get(path) {
            fs::write(target, asset.data.as_ref())?;
        }
    }
    Ok(())
}

fn safe_system_resource_target(path: &std::path::Path) -> bool {
    if path.as_os_str().is_empty() {
        return false;
    }
    !matches!(
        path.components().next_back(),
        None | Some(Component::RootDir | Component::Prefix(_))
    )
}

#[cfg(test)]
fn embedded_system_skill_relative_path(path: &str) -> Option<PathBuf> {
    embedded_system_resource_relative_path(path, "system-skills")
}

#[cfg(test)]
fn embedded_system_agent_relative_path(path: &str) -> Option<PathBuf> {
    embedded_system_resource_relative_path(path, "system-agents")
}

fn embedded_system_resource_relative_path(path: &str, out_dir_name: &str) -> Option<PathBuf> {
    let path = FsPath::new(path);
    let relative = if path.is_absolute() {
        path.strip_prefix(FsPath::new(env!("OUT_DIR")).join(out_dir_name))
            .ok()?
    } else {
        path
    };
    let relative = relative.strip_prefix(out_dir_name).unwrap_or(relative);
    safe_embedded_relative_path_from_path(relative)
}

#[cfg(test)]
fn safe_embedded_relative_path(path: &str) -> Option<PathBuf> {
    safe_embedded_relative_path_from_path(FsPath::new(path))
}

fn safe_embedded_relative_path_from_path(path: &FsPath) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

fn github_callback_page(success: bool, title: &str, message: &str, next: &str) -> Response {
    let status = if success {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    let accent = if success { "#0b7a53" } else { "#b42318" };
    let title = html_escape(title);
    let message = html_escape(message);
    let next = html_escape(next);
    let body = format!(
        r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <meta http-equiv="refresh" content="2;url={next}">
    <title>{title}</title>
    <style>
      body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; background: #f3f6fa; color: #172033; }}
      main {{ width: min(520px, calc(100vw - 32px)); border: 1px solid #d8e0ea; border-radius: 8px; padding: 28px; background: #fff; box-shadow: 0 16px 36px rgba(22, 32, 51, 0.08); }}
      .mark {{ width: 42px; height: 42px; display: grid; place-items: center; border-radius: 8px; margin-bottom: 18px; background: color-mix(in srgb, {accent} 12%, white); color: {accent}; font-weight: 900; }}
      h1 {{ margin: 0 0 8px; font-size: 22px; }}
      p {{ margin: 0 0 20px; color: #526176; line-height: 1.5; }}
      a {{ color: #1b66d2; font-weight: 800; }}
    </style>
  </head>
  <body>
    <main>
      <div class="mark">{mark}</div>
      <h1>{title}</h1>
      <p>{message}</p>
      <a href="{next}">Return to Mai settings</a>
    </main>
  </body>
</html>"#,
        mark = if success { "OK" } else { "!" }
    );
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body))
        .expect("callback response")
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

async fn get_providers(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<ProvidersResponse>, ApiError> {
    Ok(Json(state.store.providers_response().await?))
}

async fn save_providers(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ProvidersConfigRequest>,
) -> std::result::Result<Json<ProvidersResponse>, ApiError> {
    state.store.save_providers(request).await?;
    Ok(Json(state.store.providers_response().await?))
}

async fn test_provider(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<ProviderTestRequest>,
) -> std::result::Result<Response, ApiError> {
    let result = run_provider_test(&state.store, &id, request).await;
    Ok((result.status, Json(result.response)).into_response())
}

struct ProviderTestResult {
    status: StatusCode,
    response: ProviderTestResponse,
}

async fn run_provider_test(
    store: &ConfigStore,
    provider_id: &str,
    request: ProviderTestRequest,
) -> ProviderTestResult {
    let started = Instant::now();
    let selection = match store
        .resolve_provider(Some(provider_id), request.model.as_deref())
        .await
    {
        Ok(selection) => selection,
        Err(err) => {
            let provider = store.get_provider_secret(provider_id).await.ok().flatten();
            let model = request.model.clone().or_else(|| {
                provider
                    .as_ref()
                    .map(|provider| provider.default_model.clone())
            });
            return ProviderTestResult {
                status: StatusCode::BAD_REQUEST,
                response: ProviderTestResponse {
                    ok: false,
                    provider_id: provider
                        .as_ref()
                        .map(|provider| provider.id.clone())
                        .unwrap_or_else(|| provider_id.to_string()),
                    provider_name: provider
                        .as_ref()
                        .map(|provider| provider.name.clone())
                        .unwrap_or_default(),
                    provider_kind: provider
                        .as_ref()
                        .map(|provider| provider.kind)
                        .unwrap_or_default(),
                    model: model.unwrap_or_default(),
                    base_url: provider
                        .as_ref()
                        .map(|provider| provider.base_url.clone())
                        .unwrap_or_default(),
                    latency_ms: elapsed_millis(started),
                    output_preview: String::new(),
                    usage: None,
                    error: Some(err.to_string()),
                },
            };
        }
    };

    let provider = selection.provider;
    let model = selection.model;
    let base_url = provider.base_url.clone();
    let reasoning_effort = request.reasoning_effort;
    let client = ModelClient::new();
    let resolved = client.resolve(&provider, &model, reasoning_effort.as_deref());
    let response = if request.deep && resolved.supports_continuation {
        run_provider_deep_model_test(&client, &resolved, reasoning_effort).await
    } else {
        let input = [ModelInputItem::user_text("ping")];
        let mut state = ModelTurnState::default();
        let cancellation_token = CancellationToken::new();
        consume_model_stream_to_response(
            &client,
            &resolved,
            "You are a provider connectivity test. Reply with exactly: ok",
            &input,
            &[],
            &mut state,
            &cancellation_token,
        )
        .await
    };
    let latency_ms = elapsed_millis(started);
    match response {
        Ok(response) => ProviderTestResult {
            status: StatusCode::OK,
            response: ProviderTestResponse {
                ok: true,
                provider_id: provider.id,
                provider_name: provider.name,
                provider_kind: provider.kind,
                model: model.id,
                base_url,
                latency_ms,
                output_preview: model_output_preview(&response),
                usage: response.usage,
                error: None,
            },
        },
        Err(err) => ProviderTestResult {
            status: StatusCode::OK,
            response: ProviderTestResponse {
                ok: false,
                provider_id: provider.id,
                provider_name: provider.name,
                provider_kind: provider.kind,
                model: model.id,
                base_url,
                latency_ms,
                output_preview: String::new(),
                usage: None,
                error: Some(sanitize_provider_test_error(&err, &provider.api_key)),
            },
        },
    }
}

async fn run_provider_deep_model_test(
    client: &ModelClient,
    resolved: &ResolvedProvider,
    _reasoning_effort: Option<String>,
) -> std::result::Result<ModelResponse, ModelError> {
    let cancellation_token = CancellationToken::new();
    let mut state = ModelTurnState::default();
    let first_input = vec![ModelInputItem::user_text(
        "Provider deep connectivity test, step 1. Reply exactly: ok",
    )];
    let instructions = "You are a provider connectivity test. Reply with exactly: ok";
    let first = consume_model_stream_to_response(
        client,
        resolved,
        instructions,
        &first_input,
        &[],
        &mut state,
        &cancellation_token,
    )
    .await?;
    let mut second_input = first_input;
    second_input.push(ModelInputItem::assistant_text(model_output_preview(&first)));
    second_input.push(ModelInputItem::user_text(
        "Provider deep connectivity test, step 2. Reply exactly: ok",
    ));
    state.acknowledge_history_len(2);
    consume_model_stream_to_response(
        client,
        resolved,
        instructions,
        &second_input,
        &[],
        &mut state,
        &cancellation_token,
    )
    .await
}

async fn consume_model_stream_to_response(
    client: &ModelClient,
    resolved: &ResolvedProvider,
    instructions: &str,
    input: &[ModelInputItem],
    tools: &[mai_protocol::ToolDefinition],
    state: &mut ModelTurnState,
    cancellation_token: &CancellationToken,
) -> std::result::Result<ModelResponse, ModelError> {
    let mut stream = client
        .send_turn(
            resolved,
            instructions,
            input,
            tools,
            state,
            cancellation_token,
        )
        .await?;
    let mut accumulator = ModelStreamAccumulator::default();
    while let Some(event) = stream.next().await {
        if cancellation_token.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        let event = event?;
        accumulator.push(&event);
    }
    let response = accumulator.finish()?;
    client.apply_completed_state(state, response.id.as_deref());
    Ok(response)
}

fn elapsed_millis(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn model_output_preview(response: &ModelResponse) -> String {
    let text = response
        .output
        .iter()
        .filter_map(model_output_item_text)
        .collect::<Vec<_>>()
        .join("\n");
    mai_protocol::preview(&text, 500)
}

fn model_output_item_text(item: &ModelOutputItem) -> Option<String> {
    match item {
        ModelOutputItem::Message { text } => Some(text.clone()),
        ModelOutputItem::AssistantTurn { content, .. } => content.clone(),
        ModelOutputItem::FunctionCall {
            call_id,
            name,
            raw_arguments,
            ..
        } => Some(format!("function_call {name} {call_id}: {raw_arguments}")),
        ModelOutputItem::Other { raw } => Some(raw.to_string()),
    }
}

fn sanitize_provider_test_error(err: &ModelError, api_key: &str) -> String {
    let message = match err {
        ModelError::Request { endpoint, source } => {
            format!("request to {endpoint} failed: {source}")
        }
        ModelError::Api {
            endpoint,
            status,
            body,
        } => {
            let body = mai_protocol::preview(&redact_secret(body, api_key), 1_000);
            format!("request to {endpoint} returned {status}: {body}")
        }
        ModelError::Json(err) => format!("json error: {err}"),
        ModelError::Stream(message) => format!("stream error: {message}"),
        ModelError::Cancelled => "request cancelled".to_string(),
    };
    mai_protocol::preview(&redact_secret(&message, api_key), 1_500)
}

fn redact_secret(value: &str, secret: &str) -> String {
    if secret.trim().is_empty() {
        value.to_string()
    } else {
        value.replace(secret, "[redacted]")
    }
}

async fn get_mcp_servers(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<McpServersConfigRequest>, ApiError> {
    Ok(Json(McpServersConfigRequest {
        servers: state.store.list_mcp_servers().await?,
    }))
}

async fn save_mcp_servers(
    State(state): State<Arc<AppState>>,
    Json(request): Json<McpServersConfigRequest>,
) -> std::result::Result<Json<McpServersConfigRequest>, ApiError> {
    state.store.save_mcp_servers(&request.servers).await?;
    Ok(Json(McpServersConfigRequest {
        servers: state.store.list_mcp_servers().await?,
    }))
}

async fn list_git_accounts(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<GitAccountsResponse>, ApiError> {
    Ok(Json(state.runtime.list_git_accounts().await?))
}

async fn save_git_account(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GitAccountRequest>,
) -> std::result::Result<Json<GitAccountResponse>, ApiError> {
    Ok(Json(state.runtime.save_git_account(request).await?))
}

async fn save_git_account_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(mut request): Json<GitAccountRequest>,
) -> std::result::Result<Json<GitAccountResponse>, ApiError> {
    request.id = Some(id);
    Ok(Json(state.runtime.save_git_account(request).await?))
}

async fn verify_git_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> std::result::Result<Json<GitAccountResponse>, ApiError> {
    Ok(Json(GitAccountResponse {
        account: state.runtime.verify_git_account(&id).await?,
    }))
}

async fn delete_git_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> std::result::Result<Json<GitAccountsResponse>, ApiError> {
    Ok(Json(state.runtime.delete_git_account(&id).await?))
}

async fn set_default_git_account(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GitAccountDefaultRequest>,
) -> std::result::Result<Json<GitAccountsResponse>, ApiError> {
    Ok(Json(
        state
            .runtime
            .set_default_git_account(&request.account_id)
            .await?,
    ))
}

async fn list_git_account_repositories(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> std::result::Result<Json<GithubRepositoriesResponse>, ApiError> {
    Ok(Json(
        state.runtime.list_git_account_repositories(&id).await?,
    ))
}

async fn list_git_account_repository_packages(
    State(state): State<Arc<AppState>>,
    Path((id, owner, repo)): Path<(String, String, String)>,
) -> std::result::Result<Json<RepositoryPackagesResponse>, ApiError> {
    Ok(Json(
        state
            .runtime
            .list_git_account_repository_packages(&id, &owner, &repo)
            .await?,
    ))
}

async fn get_runtime_defaults(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<RuntimeDefaultsResponse>, ApiError> {
    Ok(Json(state.runtime.runtime_defaults()))
}

async fn get_github_settings(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<GithubSettingsResponse>, ApiError> {
    Ok(Json(state.store.get_github_settings().await?))
}

async fn save_github_settings(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GithubSettingsRequest>,
) -> std::result::Result<Json<GithubSettingsResponse>, ApiError> {
    let token = request.token.as_deref().unwrap_or("").trim().to_string();
    if token.is_empty() {
        Ok(Json(state.store.clear_github_token().await?))
    } else {
        Ok(Json(state.store.save_github_token(&token).await?))
    }
}

async fn get_github_app_settings(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<GithubAppSettingsResponse>, ApiError> {
    if let Some(relay) = &state.relay {
        return Ok(Json(relay.github_app_settings().await?));
    }
    Ok(Json(state.runtime.github_app_settings().await?))
}

async fn save_github_app_settings(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GithubAppSettingsRequest>,
) -> std::result::Result<Json<GithubAppSettingsResponse>, ApiError> {
    Ok(Json(state.runtime.save_github_app_settings(request).await?))
}

async fn start_github_app_manifest(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GithubAppManifestStartRequest>,
) -> std::result::Result<Json<GithubAppManifestStartResponse>, ApiError> {
    if let Some(relay) = &state.relay {
        return Ok(Json(relay.start_github_app_manifest(request).await?));
    }
    Ok(Json(
        state.runtime.start_github_app_manifest(request).await?,
    ))
}

async fn complete_github_app_manifest(
    State(state): State<Arc<AppState>>,
    Query(query): Query<GithubManifestCallbackQuery>,
) -> Response {
    if let Some(error) = query.error {
        let message = query.error_description.unwrap_or(error);
        return github_callback_page(
            false,
            "GitHub App setup was cancelled",
            &message,
            "/#settings=integrations&github-app=error",
        );
    }
    let code = query.code.unwrap_or_default();
    let state_value = query.state.unwrap_or_default();
    match state
        .runtime
        .complete_github_app_manifest(&code, &state_value)
        .await
    {
        Ok(_) => github_callback_page(
            true,
            "GitHub App connected",
            "Mai saved the GitHub App ID and private key server-side.",
            "/#settings=integrations&github-app=configured",
        ),
        Err(error) => github_callback_page(
            false,
            "GitHub App setup failed",
            &error.to_string(),
            "/#settings=integrations&github-app=error",
        ),
    }
}

async fn github_app_installation_callback(
    Query(query): Query<GithubInstallationCallbackQuery>,
) -> Response {
    let message = match (query.setup_action.as_deref(), query.installation_id) {
        (Some(action), Some(id)) => format!("GitHub App installation {action}: {id}"),
        (Some(action), None) => format!("GitHub App installation {action}."),
        (None, Some(id)) => format!("GitHub App installation ready: {id}"),
        (None, None) => "GitHub App installation finished.".to_string(),
    };
    github_callback_page(
        true,
        "GitHub App installation updated",
        &message,
        "/#settings=integrations&github-app=installed",
    )
}

async fn start_github_app_installation(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GithubAppInstallationStartRequest>,
) -> std::result::Result<Json<GithubAppInstallationStartResponse>, ApiError> {
    let relay = state.relay.as_ref().ok_or_else(|| {
        ApiError::bad_request("GitHub App installation requires relay mode".to_string())
    })?;
    Ok(Json(relay.start_github_app_installation(request).await?))
}

async fn list_github_installations(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<GithubInstallationsResponse>, ApiError> {
    if let Some(relay) = &state.relay {
        return Ok(Json(relay.list_github_installations().await?));
    }
    Ok(Json(state.runtime.list_github_installations().await?))
}

async fn refresh_github_installations(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<GithubInstallationsResponse>, ApiError> {
    if let Some(relay) = &state.relay {
        return Ok(Json(relay.list_github_installations().await?));
    }
    Ok(Json(state.runtime.refresh_github_installations().await?))
}

async fn list_github_repositories(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> std::result::Result<Json<GithubRepositoriesResponse>, ApiError> {
    if let Some(relay) = &state.relay {
        return Ok(Json(relay.list_github_repositories(id).await?));
    }
    Ok(Json(state.runtime.list_github_repositories(id).await?))
}

async fn list_github_repository_packages(
    State(state): State<Arc<AppState>>,
    Path((id, owner, repo)): Path<(u64, String, String)>,
) -> std::result::Result<Json<RepositoryPackagesResponse>, ApiError> {
    let request = GithubAppInstallationPackagesRequest {
        installation_id: id,
        owner,
        repo,
    };
    if let Some(relay) = &state.relay {
        return Ok(Json(relay.list_github_repository_packages(request).await?));
    }
    Ok(Json(
        state
            .runtime
            .list_github_installation_repository_packages(
                request.installation_id,
                &request.owner,
                &request.repo,
            )
            .await?,
    ))
}

async fn get_relay_status(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<RelayStatusResponse>, ApiError> {
    Ok(Json(match &state.relay {
        Some(relay) => relay.status().await,
        None => RelayStatusResponse {
            enabled: false,
            connected: false,
            relay_url: None,
            node_id: None,
            last_heartbeat_at: None,
            queued_deliveries: None,
            message: Some("relay disabled".to_string()),
        },
    }))
}

async fn list_skills(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<SkillsListResponse>, ApiError> {
    Ok(Json(state.runtime.list_skills().await?))
}

async fn list_agent_profiles(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<AgentProfilesResponse>, ApiError> {
    Ok(Json(state.runtime.list_agent_profiles().await?))
}

async fn save_skills_config(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SkillsConfigRequest>,
) -> std::result::Result<Json<SkillsListResponse>, ApiError> {
    Ok(Json(state.runtime.update_skills_config(request).await?))
}

async fn get_agent_config(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<AgentConfigResponse>, ApiError> {
    Ok(Json(state.runtime.agent_config().await?))
}

async fn save_agent_config(
    State(state): State<Arc<AppState>>,
    Json(request): Json<AgentConfigRequest>,
) -> std::result::Result<Json<AgentConfigResponse>, ApiError> {
    Ok(Json(state.runtime.update_agent_config(request).await?))
}

async fn get_provider_presets(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<ProviderPresetsResponse>, ApiError> {
    Ok(Json(state.store.provider_presets_response()))
}

async fn list_agents(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<Vec<mai_protocol::AgentSummary>>, ApiError> {
    Ok(Json(state.runtime.list_agents().await))
}

async fn list_tasks(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<Vec<mai_protocol::TaskSummary>>, ApiError> {
    Ok(Json(state.runtime.list_tasks().await))
}

async fn list_projects(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<Vec<mai_protocol::ProjectSummary>>, ApiError> {
    Ok(Json(state.runtime.list_projects().await))
}

async fn ensure_default_task(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<Option<mai_protocol::TaskSummary>>, ApiError> {
    Ok(Json(state.runtime.ensure_default_task().await?))
}

async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateTaskRequest>,
) -> std::result::Result<Json<CreateTaskResponse>, ApiError> {
    let task = state
        .runtime
        .create_task(request.title, request.message, request.docker_image)
        .await?;
    Ok(Json(CreateTaskResponse { task }))
}

async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<TaskId>,
    Query(query): Query<TaskDetailQuery>,
) -> std::result::Result<Json<mai_protocol::TaskDetail>, ApiError> {
    Ok(Json(state.runtime.get_task(id, query.agent_id).await?))
}

async fn send_task_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<TaskId>,
    Json(request): Json<SendMessageRequest>,
) -> std::result::Result<Json<SendMessageResponse>, ApiError> {
    let turn_id = state
        .runtime
        .send_task_message(id, request.message, request.skill_mentions)
        .await?;
    Ok(Json(SendMessageResponse { turn_id }))
}

async fn approve_task_plan(
    State(state): State<Arc<AppState>>,
    Path(id): Path<TaskId>,
) -> std::result::Result<Json<ApproveTaskPlanResponse>, ApiError> {
    let task = state.runtime.approve_task_plan(id).await?;
    Ok(Json(ApproveTaskPlanResponse { task }))
}

async fn request_plan_revision(
    State(state): State<Arc<AppState>>,
    Path(id): Path<TaskId>,
    Json(request): Json<RequestPlanRevisionRequest>,
) -> std::result::Result<Json<RequestPlanRevisionResponse>, ApiError> {
    let task = state
        .runtime
        .request_plan_revision(id, request.feedback)
        .await?;
    Ok(Json(RequestPlanRevisionResponse { task }))
}

async fn cancel_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<TaskId>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.cancel_task(id).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn delete_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<TaskId>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.delete_task(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_project(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateProjectRequest>,
) -> std::result::Result<Json<CreateProjectResponse>, ApiError> {
    let project = state.runtime.create_project(request).await?;
    Ok(Json(CreateProjectResponse { project }))
}

async fn get_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
    Query(query): Query<ProjectDetailQuery>,
) -> std::result::Result<Json<mai_protocol::ProjectDetail>, ApiError> {
    Ok(Json(
        state
            .runtime
            .get_project(id, query.agent_id, query.session_id)
            .await?,
    ))
}

async fn update_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
    Json(request): Json<UpdateProjectRequest>,
) -> std::result::Result<Json<UpdateProjectResponse>, ApiError> {
    let project = state.runtime.update_project(id, request).await?;
    Ok(Json(UpdateProjectResponse { project }))
}

async fn send_project_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
    Json(request): Json<SendMessageRequest>,
) -> std::result::Result<Json<SendMessageResponse>, ApiError> {
    let turn_id = state.runtime.send_project_message(id, request).await?;
    Ok(Json(SendMessageResponse { turn_id }))
}

async fn list_project_review_runs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
    Query(query): Query<ProjectReviewRunsQuery>,
) -> std::result::Result<Json<ProjectReviewRunsResponse>, ApiError> {
    Ok(Json(
        state
            .runtime
            .list_project_review_runs(id, query.offset.unwrap_or(0), query.limit.unwrap_or(50))
            .await?,
    ))
}

async fn get_project_review_run(
    State(state): State<Arc<AppState>>,
    Path((id, run_id)): Path<(ProjectId, String)>,
) -> std::result::Result<Json<ProjectReviewRunDetail>, ApiError> {
    let run_id = run_id.parse().map_err(|err| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: format!("invalid review run id: {err}"),
    })?;
    Ok(Json(
        state.runtime.get_project_review_run(id, run_id).await?,
    ))
}

async fn list_project_skills(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
) -> std::result::Result<Json<SkillsListResponse>, ApiError> {
    Ok(Json(state.runtime.list_project_skills(id).await?))
}

async fn detect_project_skills(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
) -> std::result::Result<Json<SkillsListResponse>, ApiError> {
    Ok(Json(state.runtime.detect_project_skills(id).await?))
}

async fn cancel_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.cancel_project(id).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.delete_project(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_agent(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateAgentRequest>,
) -> std::result::Result<Json<CreateAgentResponse>, ApiError> {
    let agent = state.runtime.create_agent(request).await?;
    Ok(Json(CreateAgentResponse { agent }))
}

async fn get_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
    Query(query): Query<AgentDetailQuery>,
) -> std::result::Result<Json<mai_protocol::AgentDetail>, ApiError> {
    Ok(Json(state.runtime.get_agent(id, query.session_id).await?))
}

async fn update_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
    Json(request): Json<UpdateAgentRequest>,
) -> std::result::Result<Json<UpdateAgentResponse>, ApiError> {
    let agent = state.runtime.update_agent(id, request).await?;
    Ok(Json(UpdateAgentResponse { agent }))
}

async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
    Json(request): Json<SendMessageRequest>,
) -> std::result::Result<Json<SendMessageResponse>, ApiError> {
    let turn_id = state
        .runtime
        .send_message(id, None, request.message, request.skill_mentions)
        .await?;
    Ok(Json(SendMessageResponse { turn_id }))
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
) -> std::result::Result<Json<CreateSessionResponse>, ApiError> {
    let session = state.runtime.create_session(id).await?;
    Ok(Json(CreateSessionResponse { session }))
}

async fn send_session_message(
    State(state): State<Arc<AppState>>,
    Path((id, session_id)): Path<(AgentId, SessionId)>,
    Json(request): Json<SendMessageRequest>,
) -> std::result::Result<Json<SendMessageResponse>, ApiError> {
    let turn_id = state
        .runtime
        .send_message(
            id,
            Some(session_id),
            request.message,
            request.skill_mentions,
        )
        .await?;
    Ok(Json(SendMessageResponse { turn_id }))
}

async fn list_agent_logs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
    Query(query): Query<AgentLogsQuery>,
) -> std::result::Result<Json<AgentLogsResponse>, ApiError> {
    let limit = bounded_api_limit(query.limit, 100, 500);
    Ok(Json(
        state
            .runtime
            .agent_logs(
                id,
                AgentLogFilter {
                    session_id: query.session_id,
                    turn_id: query.turn_id,
                    level: query.level.filter(|value| !value.trim().is_empty()),
                    category: query.category.filter(|value| !value.trim().is_empty()),
                    since: query.since,
                    until: query.until,
                    offset: query.offset.unwrap_or(0),
                    limit,
                },
            )
            .await?,
    ))
}

async fn list_tool_traces(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
    Query(query): Query<ToolTraceListQuery>,
) -> std::result::Result<Json<ToolTraceListResponse>, ApiError> {
    let limit = bounded_api_limit(query.limit, 100, 500);
    Ok(Json(
        state
            .runtime
            .tool_traces(
                id,
                ToolTraceFilter {
                    session_id: query.session_id,
                    turn_id: query.turn_id,
                    offset: query.offset.unwrap_or(0),
                    limit,
                },
            )
            .await?,
    ))
}

async fn get_tool_trace(
    State(state): State<Arc<AppState>>,
    Path((id, call_id)): Path<(AgentId, String)>,
) -> std::result::Result<Json<ToolTraceDetail>, ApiError> {
    Ok(Json(state.runtime.tool_trace(id, None, call_id).await?))
}

async fn get_session_tool_trace(
    State(state): State<Arc<AppState>>,
    Path((id, session_id, call_id)): Path<(AgentId, SessionId, String)>,
) -> std::result::Result<Json<ToolTraceDetail>, ApiError> {
    Ok(Json(
        state
            .runtime
            .tool_trace(id, Some(session_id), call_id)
            .await?,
    ))
}

async fn upload_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
    Json(request): Json<FileUploadRequest>,
) -> std::result::Result<Json<FileUploadResponse>, ApiError> {
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
    Path(id): Path<AgentId>,
    Query(query): Query<DownloadQuery>,
) -> std::result::Result<Response, ApiError> {
    let bytes = state.runtime.download_file_tar(id, query.path).await?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-tar")
        .body(Body::from(bytes))
        .expect("response builder"))
}

async fn list_artifacts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<TaskId>,
) -> std::result::Result<Json<Vec<ArtifactInfo>>, ApiError> {
    let artifacts = state.store.load_artifacts(&id).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: e.to_string(),
    })?;
    Ok(Json(artifacts))
}

async fn download_artifact(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    let artifacts = state.store.load_all_artifacts().map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: e.to_string(),
    })?;
    let artifact = artifacts
        .into_iter()
        .find(|a| a.id == id.as_str())
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            message: "Artifact not found".to_string(),
        })?;

    let file_path = state.runtime.artifact_file_path(&artifact);

    let bytes = tokio::fs::read(&file_path).await.map_err(|e| ApiError {
        status: StatusCode::NOT_FOUND,
        message: format!("File not found: {e}"),
    })?;

    let filename = content_disposition_filename(&artifact.name);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_DISPOSITION, filename)
        .body(Body::from(bytes))
        .map_err(|error| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        })?)
}

async fn cancel_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<AgentId>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.cancel_agent(id).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn cancel_agent_turn(
    State(state): State<Arc<AppState>>,
    Path((id, turn_id)): Path<(AgentId, TurnId)>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.cancel_agent_turn(id, turn_id).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn cancel_agent_colon(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> std::result::Result<StatusCode, ApiError> {
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
    Path(id): Path<AgentId>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.delete_agent(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn bounded_api_limit(limit: Option<usize>, default: usize, max: usize) -> usize {
    limit.unwrap_or(default).clamp(1, max)
}

fn content_disposition_filename(name: &str) -> String {
    let escaped = name
        .chars()
        .map(|ch| match ch {
            '"' | '\\' | '\r' | '\n' => '_',
            ch if ch.is_control() || !ch.is_ascii() => '_',
            ch => ch,
        })
        .collect::<String>();
    format!("attachment; filename=\"{escaped}\"")
}

async fn events(
    State(state): State<Arc<AppState>>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> std::result::Result<
    Sse<impl futures::Stream<Item = std::result::Result<Event, Infallible>>>,
    ApiError,
> {
    let initial = once(Ok(Event::default().comment("connected")));
    let last_event_id = query.last_event_id.or_else(|| {
        headers
            .get("last-event-id")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
    });
    let replay = if let Some(last_event_id) = last_event_id {
        state
            .store
            .service_events_after(last_event_id, SSE_REPLAY_LIMIT)
            .await?
    } else {
        Vec::new()
    };
    let replay = tokio_stream::iter(replay.into_iter().map(|event| Ok(sse_event(event))));
    let events = BroadcastStream::new(state.runtime.subscribe()).filter_map(|event| async move {
        match event {
            Ok(event) => Some(Ok(sse_event(event))),
            Err(err) => {
                tracing::warn!("SSE broadcast lagged or closed: {err}");
                None
            }
        }
    });
    let stream = initial.chain(replay).chain(events);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
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
        mai_protocol::ServiceEventKind::AgentUpdated { .. } => "agent_updated",
        mai_protocol::ServiceEventKind::AgentDeleted { .. } => "agent_deleted",
        mai_protocol::ServiceEventKind::TaskCreated { .. } => "task_created",
        mai_protocol::ServiceEventKind::TaskUpdated { .. } => "task_updated",
        mai_protocol::ServiceEventKind::TaskDeleted { .. } => "task_deleted",
        mai_protocol::ServiceEventKind::ProjectCreated { .. } => "project_created",
        mai_protocol::ServiceEventKind::ProjectUpdated { .. } => "project_updated",
        mai_protocol::ServiceEventKind::ProjectDeleted { .. } => "project_deleted",
        mai_protocol::ServiceEventKind::GithubWebhookReceived { .. } => "github_webhook_received",
        mai_protocol::ServiceEventKind::ProjectReviewQueued { .. } => "project_review_queued",
        mai_protocol::ServiceEventKind::TurnStarted { .. } => "turn_started",
        mai_protocol::ServiceEventKind::TurnCompleted { .. } => "turn_completed",
        mai_protocol::ServiceEventKind::ToolStarted { .. } => "tool_started",
        mai_protocol::ServiceEventKind::ToolCompleted { .. } => "tool_completed",
        mai_protocol::ServiceEventKind::ContextCompacted { .. } => "context_compacted",
        mai_protocol::ServiceEventKind::AgentMessage { .. } => "agent_message",
        mai_protocol::ServiceEventKind::AgentMessageDelta { .. } => "agent_message_delta",
        mai_protocol::ServiceEventKind::AgentMessageCompleted { .. } => "agent_message_completed",
        mai_protocol::ServiceEventKind::ReasoningDelta { .. } => "reasoning_delta",
        mai_protocol::ServiceEventKind::ReasoningCompleted { .. } => "reasoning_completed",
        mai_protocol::ServiceEventKind::ToolCallDelta { .. } => "tool_call_delta",
        mai_protocol::ServiceEventKind::SkillsActivated { .. } => "skills_activated",
        mai_protocol::ServiceEventKind::McpServerStatusChanged { .. } => {
            "mcp_server_status_changed"
        }
        mai_protocol::ServiceEventKind::Error { .. } => "error",
        mai_protocol::ServiceEventKind::TodoListUpdated { .. } => "todo_list_updated",
        mai_protocol::ServiceEventKind::PlanUpdated { .. } => "plan_updated",
        mai_protocol::ServiceEventKind::UserInputRequested { .. } => "user_input_requested",
        mai_protocol::ServiceEventKind::ArtifactCreated { .. } => "artifact_created",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{
        AgentId, ModelCapabilities, ModelConfig, ModelReasoningConfig, ModelReasoningVariant,
        ModelRequestPolicy, ModelWireApi, ProviderConfig, ProviderKind, ProvidersConfigRequest,
        ServiceEvent, ServiceEventKind, SessionId, SkillActivationInfo, SkillScope, TurnId,
    };
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, VecDeque};
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Mutex as TokioMutex;

    #[test]
    fn embedded_system_skills_release_to_target_dir() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("system-skills");

        release_embedded_system_skills(&target).expect("release skills");

        let skill_path = target.join("reviewer-agent-review-pr").join("SKILL.md");
        let contents = fs::read_to_string(skill_path).expect("skill contents");
        assert!(contents.contains("name: reviewer-agent-review-pr"));
    }

    #[test]
    fn embedded_system_agents_release_to_target_dir() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("system-agents");

        release_embedded_system_agents(&target).expect("release agents");

        let maintainer_path = target.join("project-maintainer").join("AGENT.md");
        let reviewer_path = target.join("project-reviewer").join("AGENT.md");
        let contents = fs::read_to_string(maintainer_path).expect("agent contents");
        assert!(contents.contains("id: project-maintainer"));
        assert!(reviewer_path.exists());
    }

    #[test]
    fn embedded_system_skills_release_overwrites_target_dir() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("system-skills");
        fs::create_dir_all(&target).expect("mkdir");
        fs::write(target.join("stale.txt"), "old").expect("write stale");

        release_embedded_system_skills(&target).expect("release skills");

        assert!(!target.join("stale.txt").exists());
        let expected = target.join("reviewer-agent-review-pr").join("SKILL.md");
        assert!(
            expected.exists(),
            "expected {}, found {:?}",
            expected.display(),
            list_relative_files(&target)
        );
    }

    fn list_relative_files(root: &FsPath) -> Vec<PathBuf> {
        let mut files = Vec::new();
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                collect_relative_files(root, &entry.path(), &mut files);
            }
        }
        files.sort();
        files
    }

    fn collect_relative_files(root: &FsPath, path: &FsPath, files: &mut Vec<PathBuf>) {
        if path.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    collect_relative_files(root, &entry.path(), files);
                }
            }
        } else if let Ok(relative) = path.strip_prefix(root) {
            files.push(relative.to_path_buf());
        }
    }

    #[test]
    fn safe_embedded_relative_path_rejects_parent_components() {
        assert_eq!(
            safe_embedded_relative_path("reviewer-agent-review-pr/SKILL.md"),
            Some(PathBuf::from("reviewer-agent-review-pr").join("SKILL.md"))
        );
        assert_eq!(
            embedded_system_skill_relative_path("system-skills/reviewer-agent-review-pr/SKILL.md"),
            Some(PathBuf::from("reviewer-agent-review-pr").join("SKILL.md"))
        );
        assert_eq!(safe_embedded_relative_path("../SKILL.md"), None);
        assert_eq!(safe_embedded_relative_path("/tmp/SKILL.md"), None);
        assert_eq!(
            embedded_system_skill_relative_path(
                &FsPath::new(env!("OUT_DIR"))
                    .join("system-skills")
                    .join("reviewer-agent-review-pr")
                    .join("SKILL.md")
                    .to_string_lossy()
            ),
            Some(PathBuf::from("reviewer-agent-review-pr").join("SKILL.md"))
        );
        assert_eq!(
            embedded_system_agent_relative_path(
                &FsPath::new(env!("OUT_DIR"))
                    .join("system-agents")
                    .join("project-maintainer")
                    .join("AGENT.md")
                    .to_string_lossy()
            ),
            Some(PathBuf::from("project-maintainer").join("AGENT.md"))
        );
    }

    #[test]
    fn system_skills_release_rejects_root_target() {
        assert!(!safe_system_resource_target(std::path::Path::new("")));
        assert!(!safe_system_resource_target(std::path::Path::new("/")));
        assert!(safe_system_resource_target(std::path::Path::new(
            "/tmp/system-skills"
        )));
    }

    #[test]
    fn runtime_storage_paths_use_default_data_layout() {
        let dir = tempdir().expect("tempdir");
        let data_dir = dir.path().join(".mai-team");

        assert_eq!(data_dir_path_with(dir.path(), None), data_dir);
        assert_eq!(cache_dir_path(&data_dir), data_dir.join("cache"));
        assert_eq!(
            artifact_files_root(&data_dir),
            data_dir.join("artifacts").join("files")
        );
        assert_eq!(
            artifact_index_root(&data_dir),
            data_dir.join("artifacts").join("index")
        );
        assert_eq!(
            system_skills_path(&data_dir),
            data_dir.join("system-skills")
        );
        assert_eq!(
            system_agents_path(&data_dir),
            data_dir.join("system-agents")
        );
    }

    #[test]
    fn runtime_storage_paths_use_cli_data_path() {
        let dir = tempdir().expect("tempdir");
        let data_dir = dir.path().join("data-root");

        assert_eq!(
            data_dir_path_with(dir.path(), Some(data_dir.clone())),
            data_dir
        );
        assert_eq!(cache_dir_path(&data_dir), data_dir.join("cache"));
    }

    #[test]
    fn cli_parses_data_path() {
        let cli =
            Cli::try_parse_from(["mai-server", "--data-path", "/tmp/mai-data"]).expect("parse cli");
        assert_eq!(cli.data_path, Some(PathBuf::from("/tmp/mai-data")));

        let cli =
            Cli::try_parse_from(["mai-server", "--data-path=/tmp/mai-data"]).expect("parse cli");
        assert_eq!(cli.data_path, Some(PathBuf::from("/tmp/mai-data")));
    }

    #[test]
    fn cli_rejects_invalid_data_path_usage() {
        assert!(Cli::try_parse_from(["mai-server", "--data-path"]).is_err());
        assert!(Cli::try_parse_from(["mai-server", "--unknown"]).is_err());
        assert!(Cli::try_parse_from(["mai-server", "--help"]).is_err());
    }

    #[test]
    fn relay_url_prefers_public_url_and_trims_trailing_slash() {
        assert_eq!(
            relay_url_from_env_values(
                Some("https://relay.example.com/"),
                Some("http://legacy.example.com")
            ),
            "https://relay.example.com"
        );
        assert_eq!(
            relay_url_from_env_values(None, Some("http://legacy.example.com/")),
            "http://legacy.example.com"
        );
        assert_eq!(
            relay_url_from_env_values(Some("  "), None),
            "http://127.0.0.1:8090"
        );
    }

    #[test]
    fn skills_activated_event_has_sse_name() {
        let event = ServiceEvent {
            sequence: 1,
            timestamp: mai_protocol::now(),
            kind: ServiceEventKind::SkillsActivated {
                agent_id: AgentId::new_v4(),
                session_id: Some(SessionId::new_v4()),
                turn_id: TurnId::new_v4(),
                skills: vec![SkillActivationInfo {
                    name: "demo".to_string(),
                    display_name: Some("Demo".to_string()),
                    path: std::path::PathBuf::from("/tmp/demo/SKILL.md"),
                    scope: SkillScope::Project,
                }],
            },
        };

        assert_eq!(event_name(&event), "skills_activated");
    }

    #[test]
    fn plan_updated_event_has_sse_name() {
        let event = ServiceEvent {
            sequence: 1,
            timestamp: mai_protocol::now(),
            kind: ServiceEventKind::PlanUpdated {
                task_id: TaskId::new_v4(),
                plan: mai_protocol::TaskPlan::default(),
            },
        };

        assert_eq!(event_name(&event), "plan_updated");
    }

    #[tokio::test]
    async fn service_event_replay_returns_events_after_sequence() {
        let dir = tempdir().expect("tempdir");
        let store = ConfigStore::open_with_config_path(
            dir.path().join("server.sqlite3"),
            dir.path().join("config.toml"),
        )
        .await
        .expect("open store");
        for sequence in 1..=3 {
            store
                .append_service_event(&ServiceEvent {
                    sequence,
                    timestamp: mai_protocol::now(),
                    kind: ServiceEventKind::Error {
                        agent_id: None,
                        session_id: None,
                        turn_id: None,
                        message: format!("event {sequence}"),
                    },
                })
                .await
                .expect("append event");
        }

        let replay = store.service_events_after(1, 10).await.expect("replay");
        assert_eq!(
            replay
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[tokio::test]
    async fn provider_test_succeeds_against_mock_responses_server() {
        let (base_url, requests) = start_provider_mock(vec![
            json!({
                "id": "resp_test_1",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "ok" }]
                    }
                ],
                "usage": { "input_tokens": 3, "output_tokens": 2, "total_tokens": 5 }
            }),
            json!({
                "id": "resp_test_2",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "ok" }]
                    }
                ],
                "usage": { "input_tokens": 4, "output_tokens": 2, "total_tokens": 6 }
            }),
        ])
        .await;
        let (_dir, store) = provider_test_store(provider_config(&base_url, Some("secret"))).await;

        let response = run_provider_test(
            &store,
            "openai",
            ProviderTestRequest {
                model: None,
                reasoning_effort: Some("minimal".to_string()),
                deep: true,
            },
        )
        .await;

        assert_eq!(response.status, StatusCode::OK);
        let response = response.response;
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.provider_id, "openai");
        assert_eq!(response.provider_name, "OpenAI");
        assert_eq!(response.provider_kind, ProviderKind::Openai);
        assert_eq!(response.model, "gpt-5.5");
        assert_eq!(response.base_url, base_url);
        assert_eq!(response.output_preview, "ok");
        assert_eq!(response.usage.expect("usage").total_tokens, 6);
        assert_eq!(response.error, None);

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["path"], "/responses");
        assert_eq!(requests[0]["authorization"], "Bearer secret");
        assert_eq!(requests[0]["body"]["model"], "gpt-5.5");
        assert_eq!(requests[0]["body"]["store"], true);
        assert_eq!(
            requests[0]["body"].pointer("/reasoning/effort"),
            Some(&json!("minimal"))
        );
        assert_eq!(requests[1]["body"]["previous_response_id"], "resp_test_1");
        assert_eq!(
            requests[1]["body"].pointer("/reasoning/effort"),
            Some(&json!("minimal"))
        );
    }

    #[tokio::test]
    async fn provider_test_deep_mode_covers_continuation_fallback() {
        let (base_url, requests) = start_provider_mock(vec![
            json!({
                "id": "resp_test_1",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "ok" }]
                    }
                ],
                "usage": { "input_tokens": 3, "output_tokens": 2, "total_tokens": 5 }
            }),
            json!({
                "__status": 400,
                "error": {
                    "message": "previous_response_id is only supported on Responses WebSocket v2",
                    "type": "invalid_request_error"
                }
            }),
            json!({
                "id": "resp_test_2",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "ok" }]
                    }
                ],
                "usage": { "input_tokens": 6, "output_tokens": 2, "total_tokens": 8 }
            }),
        ])
        .await;
        let (_dir, store) = provider_test_store(provider_config(&base_url, Some("secret"))).await;

        let response = run_provider_test(&store, "openai", ProviderTestRequest::default()).await;

        assert_eq!(response.status, StatusCode::OK);
        let response = response.response;
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.output_preview, "ok");
        assert_eq!(response.usage.expect("usage").total_tokens, 8);

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert!(requests[0]["body"].get("previous_response_id").is_none());
        assert_eq!(requests[1]["body"]["previous_response_id"], "resp_test_1");
        assert!(requests[2]["body"].get("previous_response_id").is_none());
        assert_eq!(requests[2]["body"]["store"], false);
        assert_eq!(
            requests[2]["body"]["input"]
                .as_array()
                .expect("input")
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn provider_test_reports_missing_provider() {
        let (_dir, store) =
            provider_test_store(provider_config("http://127.0.0.1:1", Some("secret"))).await;

        let response = run_provider_test(&store, "missing", ProviderTestRequest::default()).await;

        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let response = response.response;
        assert!(!response.ok);
        assert_eq!(response.provider_id, "missing");
        assert!(
            response
                .error
                .unwrap()
                .contains("provider `missing` not found")
        );
    }

    #[tokio::test]
    async fn provider_test_reports_missing_api_key_with_provider_context() {
        let (_dir, store) = provider_test_store(provider_config("http://127.0.0.1:1", None)).await;

        let response = run_provider_test(&store, "openai", ProviderTestRequest::default()).await;

        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let response = response.response;
        assert!(!response.ok);
        assert_eq!(response.provider_id, "openai");
        assert_eq!(response.provider_name, "OpenAI");
        assert_eq!(response.model, "gpt-5.5");
        assert_eq!(response.base_url, "http://127.0.0.1:1");
        assert!(
            response
                .error
                .unwrap()
                .contains("provider `openai` has no API key")
        );
    }

    #[tokio::test]
    async fn provider_test_reports_unknown_model_with_provider_context() {
        let (_dir, store) =
            provider_test_store(provider_config("http://127.0.0.1:1", Some("secret"))).await;

        let response = run_provider_test(
            &store,
            "openai",
            ProviderTestRequest {
                model: Some("missing-model".to_string()),
                reasoning_effort: None,
                deep: true,
            },
        )
        .await;

        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let response = response.response;
        assert!(!response.ok);
        assert_eq!(response.provider_id, "openai");
        assert_eq!(response.model, "missing-model");
        assert!(
            response
                .error
                .unwrap()
                .contains("model `missing-model` is not configured for provider `openai`")
        );
    }

    #[tokio::test]
    async fn provider_test_reports_upstream_error_without_leaking_key() {
        let (base_url, _requests) = start_provider_mock(vec![json!({
            "__status": 401,
            "error": {
                "message": "bad token secret-token",
                "type": "invalid_request_error"
            }
        })])
        .await;
        let (_dir, store) =
            provider_test_store(provider_config(&base_url, Some("secret-token"))).await;

        let response = run_provider_test(&store, "openai", ProviderTestRequest::default()).await;

        assert_eq!(response.status, StatusCode::OK);
        let response = response.response;
        assert!(!response.ok);
        assert_eq!(response.base_url, base_url);
        let error = response.error.expect("error");
        assert!(error.contains("returned 401 Unauthorized"));
        assert!(error.contains("[redacted]"));
        assert!(
            !error.contains("secret-token"),
            "provider test leaked api key: {error}"
        );
    }

    async fn provider_test_store(provider: ProviderConfig) -> (tempfile::TempDir, ConfigStore) {
        let dir = tempdir().expect("tempdir");
        let store = ConfigStore::open_with_config_and_artifact_index_path(
            dir.path().join("config.sqlite3"),
            dir.path().join("config.toml"),
            dir.path().join("artifacts/index"),
        )
        .await
        .expect("open store");
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![provider],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        (dir, store)
    }

    fn provider_config(base_url: &str, api_key: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            id: "openai".to_string(),
            kind: ProviderKind::Openai,
            name: "OpenAI".to_string(),
            base_url: base_url.to_string(),
            api_key: api_key.map(str::to_string),
            api_key_env: None,
            models: vec![provider_test_model("gpt-5.5")],
            default_model: "gpt-5.5".to_string(),
            enabled: true,
        }
    }

    fn provider_test_model(id: &str) -> ModelConfig {
        ModelConfig {
            id: id.to_string(),
            name: Some(id.to_string()),
            context_tokens: 400_000,
            output_tokens: 128_000,
            supports_tools: true,
            wire_api: ModelWireApi::Responses,
            capabilities: ModelCapabilities::default(),
            request_policy: ModelRequestPolicy::default(),
            reasoning: Some(ModelReasoningConfig {
                default_variant: Some("medium".to_string()),
                variants: ["minimal", "medium"]
                    .into_iter()
                    .map(|id| ModelReasoningVariant {
                        id: id.to_string(),
                        label: None,
                        request: json!({
                            "reasoning": {
                                "effort": id
                            }
                        }),
                    })
                    .collect(),
            }),
            options: Value::Null,
            headers: BTreeMap::new(),
        }
    }

    async fn start_provider_mock(responses: Vec<Value>) -> (String, Arc<TokioMutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock addr");
        let responses = Arc::new(TokioMutex::new(VecDeque::from(responses)));
        let requests = Arc::new(TokioMutex::new(Vec::new()));
        let server_responses = Arc::clone(&responses);
        let server_requests = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let responses = Arc::clone(&server_responses);
                let requests = Arc::clone(&server_requests);
                tokio::spawn(async move {
                    let request = read_provider_mock_request(&mut stream).await;
                    requests.lock().await.push(request);
                    let response = responses.lock().await.pop_front().unwrap_or_else(|| {
                        json!({
                            "id": "resp_empty",
                            "output": [],
                            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
                        })
                    });
                    write_provider_mock_response(&mut stream, response).await;
                });
            }
        });
        (format!("http://{addr}"), requests)
    }

    async fn read_provider_mock_request(stream: &mut TcpStream) -> Value {
        let mut buffer = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let n = stream.read(&mut chunk).await.expect("read request");
            if n == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..n]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                let text = String::from_utf8_lossy(&buffer);
                let header_end = text.find("\r\n\r\n").expect("header end");
                let headers = &text[..header_end];
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let body_start = header_end + 4;
                if buffer.len() >= body_start + content_length {
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&buffer);
        let header_end = text.find("\r\n\r\n").expect("header end");
        let headers = &text[..header_end];
        let body = &buffer[header_end + 4..];
        let path = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default();
        let authorization = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .map(|(_, value)| value.trim().to_string())
            })
            .unwrap_or_default();
        json!({
            "path": path,
            "authorization": authorization,
            "body": serde_json::from_slice::<Value>(body).unwrap_or(Value::Null),
        })
    }

    async fn write_provider_mock_response(stream: &mut TcpStream, mut response: Value) {
        let status = response
            .as_object_mut()
            .and_then(|object| object.remove("__status"))
            .and_then(|value| value.as_u64())
            .unwrap_or(200);
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Status",
        };
        let body = if status == 200 {
            provider_mock_sse_body(&response)
        } else {
            serde_json::to_string(&response).expect("response json")
        };
        let content_type = if status == 200 {
            "text/event-stream"
        } else {
            "application/json"
        };
        let raw = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(raw.as_bytes())
            .await
            .expect("write response");
    }

    fn provider_mock_sse_body(response: &Value) -> String {
        let response_id = response
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("resp_mock");
        let mut events = vec![json!({
            "type": "response.created",
            "response": { "id": response_id }
        })];
        for (index, item) in response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            events.push(json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": item,
            }));
        }
        events.push(json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "usage": response.get("usage").cloned().unwrap_or(Value::Null),
            }
        }));
        events
            .into_iter()
            .map(|event| {
                let kind = event
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message");
                format!("event: {kind}\ndata: {event}\n\n")
            })
            .collect()
    }
}
