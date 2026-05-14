use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use mai_protocol::*;
use serde::Deserialize;

use super::state::{ApiError, AppState};
use crate::services::github::GithubService;

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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

#[derive(Debug, Deserialize)]
pub(crate) struct GithubManifestCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubInstallationCallbackQuery {
    setup_action: Option<String>,
    installation_id: Option<u64>,
}

fn github_service(state: &AppState) -> GithubService {
    GithubService::new(Arc::clone(&state.runtime), state.relay.clone())
}

pub(crate) async fn get_github_settings(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<GithubSettingsResponse>, ApiError> {
    Ok(Json(state.store.get_github_settings().await?))
}

pub(crate) async fn save_github_settings(
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

pub(crate) async fn get_github_app_settings(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<GithubAppSettingsResponse>, ApiError> {
    let svc = github_service(&state);
    Ok(Json(svc.app_settings().await?))
}

pub(crate) async fn save_github_app_settings(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GithubAppSettingsRequest>,
) -> std::result::Result<Json<GithubAppSettingsResponse>, ApiError> {
    let svc = github_service(&state);
    Ok(Json(svc.save_app_settings(request).await?))
}

pub(crate) async fn start_github_app_manifest(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GithubAppManifestStartRequest>,
) -> std::result::Result<Json<GithubAppManifestStartResponse>, ApiError> {
    let svc = github_service(&state);
    Ok(Json(svc.start_manifest(request).await?))
}

pub(crate) async fn complete_github_app_manifest(
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
    let svc = github_service(&state);
    match svc.complete_manifest(&code, &state_value).await {
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

pub(crate) async fn github_app_installation_callback(
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

pub(crate) async fn start_github_app_installation(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GithubAppInstallationStartRequest>,
) -> std::result::Result<Json<GithubAppInstallationStartResponse>, ApiError> {
    let svc = github_service(&state);
    Ok(Json(svc.start_installation(request).await?))
}

pub(crate) async fn list_github_installations(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<GithubInstallationsResponse>, ApiError> {
    let svc = github_service(&state);
    Ok(Json(svc.list_installations().await?))
}

pub(crate) async fn refresh_github_installations(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<GithubInstallationsResponse>, ApiError> {
    let svc = github_service(&state);
    Ok(Json(svc.refresh_installations().await?))
}

pub(crate) async fn list_github_repositories(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> std::result::Result<Json<GithubRepositoriesResponse>, ApiError> {
    let svc = github_service(&state);
    Ok(Json(svc.list_repositories(id).await?))
}

pub(crate) async fn list_github_repository_packages(
    State(state): State<Arc<AppState>>,
    Path((id, owner, repo)): Path<(u64, String, String)>,
) -> std::result::Result<Json<RepositoryPackagesResponse>, ApiError> {
    let svc = github_service(&state);
    Ok(Json(svc.list_repository_packages(id, &owner, &repo).await?))
}

pub(crate) async fn get_relay_status(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<RelayStatusResponse>, ApiError> {
    let svc = github_service(&state);
    Ok(Json(svc.relay_status().await))
}
