use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use mai_docker::DockerClient;
use mai_model::ResponsesClient;
use mai_protocol::{
    AgentConfigRequest, AgentConfigResponse, AgentId, AgentProfilesResponse,
    ApproveTaskPlanResponse, ArtifactInfo, CreateAgentRequest, CreateAgentResponse,
    CreateProjectRequest, CreateProjectResponse, CreateSessionResponse, CreateTaskRequest,
    CreateTaskResponse, ErrorResponse, FileUploadRequest, FileUploadResponse,
    GitAccountDefaultRequest, GitAccountRequest, GitAccountResponse, GitAccountsResponse,
    GithubAppManifestStartRequest, GithubAppManifestStartResponse, GithubAppSettingsRequest,
    GithubAppSettingsResponse, GithubInstallationsResponse, GithubRepositoriesResponse,
    GithubSettingsRequest, GithubSettingsResponse, McpServersConfigRequest, ProjectId,
    ProjectReviewRunDetail, ProjectReviewRunsResponse, ProviderPresetsResponse,
    ProvidersConfigRequest, ProvidersResponse, RepositoryPackagesResponse,
    RequestPlanRevisionRequest, RequestPlanRevisionResponse, RuntimeDefaultsResponse,
    SendMessageRequest, SendMessageResponse, ServiceEvent, SessionId, SkillsConfigRequest,
    SkillsListResponse, TaskId, ToolTraceDetail, TurnId, UpdateAgentRequest, UpdateAgentResponse,
    UpdateProjectRequest, UpdateProjectResponse,
};
use mai_runtime::{AgentRuntime, RuntimeConfig, RuntimeError};
use mai_store::ConfigStore;
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
use tokio_stream::once;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

#[derive(Clone)]
struct AppState {
    runtime: Arc<AgentRuntime>,
    store: Arc<ConfigStore>,
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
    let db_path = env::var("MAI_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or(ConfigStore::default_path()?);
    let config_path = env::var("MAI_CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or(ConfigStore::default_config_path()?);
    let data_dir = data_dir_path()?;
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

    let store = Arc::new(
        ConfigStore::open_with_config_and_artifact_index_path(
            db_path,
            config_path,
            &artifact_index_root,
        )
        .await?,
    );
    store
        .seed_default_provider_from_env(api_key, base_url, model)
        .await?;

    let system_skills_root = system_skills_path()?;
    release_embedded_system_skills(&system_skills_root)?;
    info!(
        path = %system_skills_root.display(),
        "released embedded system skills"
    );
    let system_agents_root = system_agents_path()?;
    release_embedded_system_agents(&system_agents_root)?;
    info!(
        path = %system_agents_root.display(),
        "released embedded system agents"
    );

    let model = ResponsesClient::new();
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
    let runtime = AgentRuntime::new(docker, model, Arc::clone(&store), runtime_config).await?;
    let cleaned = runtime.cleanup_orphaned_containers().await?;
    if !cleaned.is_empty() {
        info!(
            count = cleaned.len(),
            "removed orphaned mai-team containers"
        );
    }
    let state = Arc::new(AppState { runtime, store });

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/providers", get(get_providers).put(save_providers))
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
        .route("/github/installations", get(list_github_installations))
        .route(
            "/github/installations:refresh",
            post(refresh_github_installations),
        )
        .route(
            "/github/installations/{id}/repositories",
            get(list_github_repositories),
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

fn system_skills_path() -> Result<PathBuf> {
    env::var("MAI_SYSTEM_SKILLS_PATH")
        .map(PathBuf::from)
        .or_else(|_| {
            dirs::home_dir()
                .map(|home| home.join(".mai-team").join("system-skills"))
                .ok_or(env::VarError::NotPresent)
        })
        .context("home directory not found; set MAI_SYSTEM_SKILLS_PATH")
}

fn system_agents_path() -> Result<PathBuf> {
    env::var("MAI_SYSTEM_AGENTS_PATH")
        .map(PathBuf::from)
        .or_else(|_| {
            dirs::home_dir()
                .map(|home| home.join(".mai-team").join("system-agents"))
                .ok_or(env::VarError::NotPresent)
        })
        .context("home directory not found; set MAI_SYSTEM_AGENTS_PATH")
}

fn data_dir_path() -> Result<PathBuf> {
    data_dir_path_with(env::var_os("MAI_DATA_DIR"), dirs::home_dir())
}

fn cache_dir_path(data_dir: &std::path::Path) -> PathBuf {
    cache_dir_path_with(data_dir, env::var_os("MAI_CACHE_DIR"))
}

fn data_dir_path_with(
    env_data_dir: Option<std::ffi::OsString>,
    home_dir: Option<PathBuf>,
) -> Result<PathBuf> {
    env_data_dir
        .map(PathBuf::from)
        .or_else(|| home_dir.map(|home| home.join(".mai-team")))
        .context("home directory not found; set MAI_DATA_DIR")
}

fn cache_dir_path_with(
    data_dir: &std::path::Path,
    env_cache_dir: Option<std::ffi::OsString>,
) -> PathBuf {
    env_cache_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("cache"))
}

fn artifact_files_root(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("artifacts").join("files")
}

fn artifact_index_root(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("artifacts").join("index")
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

fn embedded_system_skill_relative_path(path: &str) -> Option<PathBuf> {
    embedded_system_resource_relative_path(path, "system-skills")
}

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

async fn list_github_installations(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<GithubInstallationsResponse>, ApiError> {
    Ok(Json(state.runtime.list_github_installations().await?))
}

async fn refresh_github_installations(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<GithubInstallationsResponse>, ApiError> {
    Ok(Json(state.runtime.refresh_github_installations().await?))
}

async fn list_github_repositories(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> std::result::Result<Json<GithubRepositoriesResponse>, ApiError> {
    Ok(Json(state.runtime.list_github_repositories(id).await?))
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

    let filename = artifact.name.clone();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(Body::from(bytes))
        .expect("response builder"))
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

async fn events(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<
    Sse<impl futures::Stream<Item = std::result::Result<Event, Infallible>>>,
    ApiError,
> {
    let initial = once(Ok(Event::default().comment("connected")));
    let events = BroadcastStream::new(state.runtime.subscribe()).filter_map(|event| async move {
        match event {
            Ok(event) => Some(Ok(sse_event(event))),
            Err(_) => None,
        }
    });
    let stream = initial.chain(events);
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
        mai_protocol::ServiceEventKind::TurnStarted { .. } => "turn_started",
        mai_protocol::ServiceEventKind::TurnCompleted { .. } => "turn_completed",
        mai_protocol::ServiceEventKind::ToolStarted { .. } => "tool_started",
        mai_protocol::ServiceEventKind::ToolCompleted { .. } => "tool_completed",
        mai_protocol::ServiceEventKind::ContextCompacted { .. } => "context_compacted",
        mai_protocol::ServiceEventKind::AgentMessage { .. } => "agent_message",
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
        AgentId, ServiceEvent, ServiceEventKind, SessionId, SkillActivationInfo, SkillScope, TurnId,
    };
    use tempfile::tempdir;

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

        assert_eq!(
            data_dir_path_with(None, Some(dir.path().to_path_buf())).expect("data dir"),
            data_dir
        );
        assert_eq!(cache_dir_path_with(&data_dir, None), data_dir.join("cache"));
        assert_eq!(
            artifact_files_root(&data_dir),
            data_dir.join("artifacts").join("files")
        );
        assert_eq!(
            artifact_index_root(&data_dir),
            data_dir.join("artifacts").join("index")
        );
    }

    #[test]
    fn runtime_storage_paths_use_env_overrides() {
        let dir = tempdir().expect("tempdir");
        let data_dir = dir.path().join("data-root");
        let cache_dir = dir.path().join("cache-root");

        assert_eq!(
            data_dir_path_with(Some(data_dir.clone().into_os_string()), None).expect("data dir"),
            data_dir
        );
        assert_eq!(
            cache_dir_path_with(&data_dir, Some(cache_dir.clone().into_os_string())),
            cache_dir
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
}
