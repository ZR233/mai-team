use crate::callback_page;
use crate::error::{RelayErrorKind, RelayResult};
use crate::state::AppState;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use mai_protocol::{
    GithubAppInstallationStartRequest, GithubAppInstallationStartResponse,
    GithubAppManifestAccountType, GithubAppManifestStartRequest, GithubAppManifestStartResponse,
};
use serde_json::Value;
use uuid::Uuid;

use super::types::{
    GithubAppConfig, GithubInstallationCallbackQuery, GithubManifestConversionResponse,
    InstallationState, ManifestState,
};

pub(crate) async fn start_manifest(
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
    let manifest =
        super::app::github_app_manifest(&redirect_url, &setup_url, &webhook_url, &webhook_secret);
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
    crate::rpc::to_value(GithubAppManifestStartResponse {
        state: state_id,
        action_url,
        manifest,
    })
}

pub(crate) async fn complete_manifest(
    state: &AppState,
    code: &str,
    state_id: &str,
) -> RelayResult<()> {
    if !is_valid_manifest_code(code) {
        return Err(RelayErrorKind::InvalidInput(
            "invalid GitHub manifest code".to_string(),
        ));
    }
    let (manifest_state, saved_webhook_secret) = state.store.take_manifest_state(state_id)?;
    let url = super::api::github_api_url(
        &state.github_api_base_url,
        &format!("/app-manifests/{code}/conversions"),
    );
    let response = state
        .http
        .post(url)
        .headers(super::api::github_headers())
        .send()
        .await?;
    let conversion: GithubManifestConversionResponse =
        super::api::decode_github_response(response, "create app from manifest").await?;
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
pub(crate) async fn start_app_installation(
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
    let install_url =
        super::app::github_app_install_url(&state.github_web_base_url, app_slug, Some(&state_id));
    Ok(GithubAppInstallationStartResponse {
        state: state_id.clone(),
        install_url,
        app: super::app::github_app_settings_response(
            &config,
            &state.github_api_base_url,
            &state.github_web_base_url,
            Some(&state_id),
        ),
    })
}

pub(crate) async fn complete_app_installation(
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
            super::api::verify_installation(state, installation_id).await?;
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

pub(crate) fn github_installation_fallback_page(
    query: &GithubInstallationCallbackQuery,
) -> Response {
    let message = match (query.setup_action.as_deref(), query.installation_id) {
        (Some(action), Some(id)) => format!("GitHub App installation {action}: {id}"),
        (Some(action), None) => format!("GitHub App installation {action}."),
        (None, Some(id)) => format!("GitHub App installation ready: {id}"),
        (None, None) => "GitHub App installation finished.".to_string(),
    };
    callback_page::callback_page(true, "GitHub App installation updated", &message)
}
pub(crate) fn local_return_url(
    origin: &str,
    return_hash: &str,
    params: &str,
) -> RelayResult<String> {
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

pub(crate) fn sanitize_origin(value: &str) -> RelayResult<String> {
    let value = value.trim().trim_end_matches('/');
    if value.starts_with("http://") || value.starts_with("https://") {
        Ok(value.to_string())
    } else {
        Err(RelayErrorKind::InvalidInput(
            "origin must start with http:// or https://".to_string(),
        ))
    }
}

pub(crate) fn is_valid_github_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub(crate) fn is_valid_manifest_code(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_return_url_is_hash_based() {
        assert_eq!(
            local_return_url(
                "http://127.0.0.1:8080",
                "#projects",
                "github-app=installed&installation_id=42"
            )
            .expect("url"),
            "http://127.0.0.1:8080/#projects?github-app=installed&installation_id=42"
        );
    }
}
