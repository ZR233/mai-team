use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::Json;
use mai_protocol::*;
use serde::Deserialize;

use super::helpers::github_callback_page;
use super::state::{ApiError, AppState};

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
    if let Some(relay) = &state.relay {
        return Ok(Json(relay.github_app_settings().await?));
    }
    Ok(Json(state.runtime.github_app_settings().await?))
}

pub(crate) async fn save_github_app_settings(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GithubAppSettingsRequest>,
) -> std::result::Result<Json<GithubAppSettingsResponse>, ApiError> {
    Ok(Json(state.runtime.save_github_app_settings(request).await?))
}

pub(crate) async fn start_github_app_manifest(
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
    let relay = state.relay.as_ref().ok_or_else(|| {
        ApiError::bad_request("GitHub App installation requires relay mode".to_string())
    })?;
    Ok(Json(relay.start_github_app_installation(request).await?))
}

pub(crate) async fn list_github_installations(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<GithubInstallationsResponse>, ApiError> {
    if let Some(relay) = &state.relay {
        return Ok(Json(relay.list_github_installations().await?));
    }
    Ok(Json(state.runtime.list_github_installations().await?))
}

pub(crate) async fn refresh_github_installations(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<GithubInstallationsResponse>, ApiError> {
    if let Some(relay) = &state.relay {
        return Ok(Json(relay.list_github_installations().await?));
    }
    Ok(Json(state.runtime.refresh_github_installations().await?))
}

pub(crate) async fn list_github_repositories(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> std::result::Result<Json<GithubRepositoriesResponse>, ApiError> {
    if let Some(relay) = &state.relay {
        return Ok(Json(relay.list_github_repositories(id).await?));
    }
    Ok(Json(state.runtime.list_github_repositories(id).await?))
}

pub(crate) async fn list_github_repository_packages(
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

pub(crate) async fn get_relay_status(
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
