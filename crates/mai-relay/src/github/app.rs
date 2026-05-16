use crate::error::{RelayErrorKind, RelayResult};
use crate::state::AppState;
use crate::store::RelayStore;
use anyhow::{Context, Result};
use mai_protocol::{GithubAppSettingsRequest, GithubAppSettingsResponse};
use serde_json::{Value, json};
use std::env;
use tracing::{info, warn};
use uuid::Uuid;

use super::types::{
    GithubAppApi, GithubAppConfig, GithubAppHookConfigRequest, GithubAppHookConfigResponse,
    GithubHookReset,
};

pub(crate) fn github_app_settings(state: &AppState) -> RelayResult<GithubAppSettingsResponse> {
    let Some(config) = state.store.github_app_config()? else {
        return Ok(GithubAppSettingsResponse {
            app_id: None,
            base_url: state.github_api_base_url.clone(),
            public_url: Some(state.public_url.clone()),
            has_private_key: false,
            app_slug: None,
            app_html_url: None,
            owner_login: None,
            owner_type: None,
            install_url: None,
        });
    };
    Ok(github_app_settings_response(
        &config,
        &state.github_api_base_url,
        &state.github_web_base_url,
        &relay_public_url(state)?,
        None,
    ))
}

pub(crate) async fn save_github_app_settings(
    state: &AppState,
    request: GithubAppSettingsRequest,
) -> RelayResult<GithubAppSettingsResponse> {
    if compiled_github_app_config()?.is_some() {
        return Err(RelayErrorKind::InvalidInput(
            "compiled GitHub App config is read-only".to_string(),
        ));
    }
    let app_id = required_setting(request.app_id, "GitHub App ID")?;
    let private_key = required_setting(request.private_key, "GitHub App private key")?;
    let public_url = required_setting(request.public_url, "relay public URL")?;
    let mut config = GithubAppConfig {
        app_id,
        private_key,
        webhook_secret: state
            .store
            .github_app_config()?
            .map(|config| config.webhook_secret.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        app_slug: request
            .app_slug
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        app_html_url: request
            .app_html_url
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        owner_login: request
            .owner_login
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        owner_type: request
            .owner_type
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
    };
    hydrate_github_app_metadata(&state.http, &state.github_api_base_url, &mut config).await?;
    update_github_app_hook_config(
        &state.http,
        &state.github_api_base_url,
        &public_url,
        &config,
    )
    .await?;
    state.store.save_relay_config(&public_url)?;
    state.store.save_github_app_config(&config)?;
    Ok(github_app_settings_response(
        &config,
        &state.github_api_base_url,
        &state.github_web_base_url,
        &public_url,
        None,
    ))
}
pub(crate) async fn bootstrap_github_app_config(
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

pub(crate) async fn hydrate_github_app_metadata(
    http: &reqwest::Client,
    github_api_base_url: &str,
    config: &mut GithubAppConfig,
) -> RelayResult<()> {
    if github_app_config_has_metadata(config) {
        return Ok(());
    }
    let jwt = super::api::github_app_jwt_for_config(config)?;
    let url = super::api::github_api_url(github_api_base_url, "/app");
    let response = http
        .get(url)
        .bearer_auth(jwt)
        .headers(super::api::github_headers())
        .send()
        .await?;
    let app =
        super::api::decode_github_response::<GithubAppApi>(response, "get GitHub App metadata")
            .await?;
    apply_github_app_metadata(config, app);
    Ok(())
}

pub(crate) fn github_app_config_has_metadata(config: &GithubAppConfig) -> bool {
    config
        .app_slug
        .as_deref()
        .is_some_and(|slug| !is_placeholder_github_app_slug(slug))
        && config.app_html_url.is_some()
        && config.owner_login.is_some()
        && config.owner_type.is_some()
}

pub(crate) fn apply_github_app_metadata(config: &mut GithubAppConfig, app: GithubAppApi) {
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

pub(crate) fn merge_github_app_config(
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

pub(crate) async fn update_github_app_hook_config(
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
    let jwt = super::api::github_app_jwt_for_config(config)?;
    let current_url = super::api::github_api_url(github_api_base_url, "/app/hook/config");
    let current = http
        .get(&current_url)
        .bearer_auth(&jwt)
        .headers(super::api::github_headers())
        .send()
        .await?;
    if current.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(GithubHookReset::Missing);
    }
    let current = super::api::decode_github_response::<GithubAppHookConfigResponse>(
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
    let url = super::api::github_api_url(github_api_base_url, "/app/hook/config");
    let body = github_app_hook_config_request(public_url, &config.webhook_secret);
    let response = http
        .patch(url)
        .bearer_auth(&jwt)
        .headers(super::api::github_headers())
        .json(&body)
        .send()
        .await?;
    super::api::decode_github_response::<Value>(response, "update GitHub App webhook config")
        .await?;
    Ok(GithubHookReset::Updated)
}

pub(crate) fn github_app_hook_config_request(
    public_url: &str,
    secret: &str,
) -> GithubAppHookConfigRequest {
    GithubAppHookConfigRequest {
        url: format!("{}/github/webhook", public_url.trim_end_matches('/')),
        content_type: "json",
        insecure_ssl: "0",
        secret: secret.to_string(),
    }
}

pub(crate) fn github_app_config_from_env() -> Result<Option<GithubAppConfig>> {
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

#[cfg(feature = "compiled-github-app-config")]
pub(crate) fn compiled_github_app_config() -> RelayResult<Option<GithubAppConfig>> {
    let app_id = option_env!("MAI_RELAY_GITHUB_APP_ID")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            RelayErrorKind::InvalidInput(
                "MAI_RELAY_GITHUB_APP_ID must be set at compile time".to_string(),
            )
        })?;
    let private_key = option_env!("MAI_RELAY_GITHUB_APP_PRIVATE_KEY")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            RelayErrorKind::InvalidInput(
                "MAI_RELAY_GITHUB_APP_PRIVATE_KEY must be set at compile time".to_string(),
            )
        })?;
    Ok(Some(GithubAppConfig {
        app_id: app_id.to_string(),
        private_key: private_key.to_string(),
        webhook_secret: String::new(),
        app_slug: compiled_optional_env("MAI_RELAY_GITHUB_APP_SLUG"),
        app_html_url: compiled_optional_env("MAI_RELAY_GITHUB_APP_HTML_URL"),
        owner_login: compiled_optional_env("MAI_RELAY_GITHUB_APP_OWNER_LOGIN"),
        owner_type: compiled_optional_env("MAI_RELAY_GITHUB_APP_OWNER_TYPE"),
    }))
}

#[cfg(not(feature = "compiled-github-app-config"))]
pub(crate) fn compiled_github_app_config() -> RelayResult<Option<GithubAppConfig>> {
    Ok(None)
}

#[cfg(feature = "compiled-github-app-config")]
fn compiled_optional_env(name: &str) -> Option<String> {
    match name {
        "MAI_RELAY_GITHUB_APP_SLUG" => option_env!("MAI_RELAY_GITHUB_APP_SLUG"),
        "MAI_RELAY_GITHUB_APP_HTML_URL" => option_env!("MAI_RELAY_GITHUB_APP_HTML_URL"),
        "MAI_RELAY_GITHUB_APP_OWNER_LOGIN" => option_env!("MAI_RELAY_GITHUB_APP_OWNER_LOGIN"),
        "MAI_RELAY_GITHUB_APP_OWNER_TYPE" => option_env!("MAI_RELAY_GITHUB_APP_OWNER_TYPE"),
        _ => None,
    }
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .map(str::to_string)
}

fn optional_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn required_setting(value: Option<String>, label: &str) -> RelayResult<String> {
    value
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| RelayErrorKind::InvalidInput(format!("{label} is required")))
}

fn relay_public_url(state: &AppState) -> RelayResult<String> {
    Ok(state.store.relay_config(&state.public_url)?.url)
}

pub(crate) fn is_placeholder_github_app_slug(value: &str) -> bool {
    value.trim() == "github-app-slug"
}
pub(crate) fn github_app_settings_response(
    config: &GithubAppConfig,
    api_base_url: &str,
    web_base_url: &str,
    public_url: &str,
    install_state: Option<&str>,
) -> GithubAppSettingsResponse {
    GithubAppSettingsResponse {
        app_id: Some(config.app_id.clone()),
        base_url: api_base_url.to_string(),
        public_url: Some(public_url.to_string()),
        has_private_key: !config.private_key.trim().is_empty(),
        app_slug: config.app_slug.clone(),
        app_html_url: config.app_html_url.clone(),
        owner_login: config.owner_login.clone(),
        owner_type: config.owner_type.clone(),
        install_url: config
            .app_slug
            .as_deref()
            .map(|slug| github_app_install_url(web_base_url, slug, install_state)),
    }
}

pub(crate) fn github_app_install_url(
    web_base_url: &str,
    app_slug: &str,
    state: Option<&str>,
) -> String {
    let base = format!(
        "{}/apps/{}/installations/select_target",
        web_base_url.trim_end_matches('/'),
        app_slug
    );
    match state {
        Some(state) => format!("{base}?state={state}"),
        None => base,
    }
}
pub(crate) fn github_app_manifest(
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
        "public": true,
        "default_permissions": {
            "contents": "write",
            "pull_requests": "write",
            "issues": "write",
            "packages": "read",
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(manifest["default_permissions"]["packages"], "read");
        assert_eq!(manifest["default_permissions"]["checks"], "read");
        assert_eq!(manifest["default_permissions"]["statuses"], "read");
        assert_eq!(manifest["default_events"][0], "pull_request");
        assert_eq!(manifest["public"], true);
        assert_eq!(manifest["webhook_secret"], "secret");
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
            "https://relay.example",
            Some("state-1"),
        );
        assert_eq!(settings.app_id.as_deref(), Some("123"));
        assert_eq!(
            settings.public_url.as_deref(),
            Some("https://relay.example")
        );
        assert!(settings.has_private_key);
        assert_eq!(
            settings.install_url.as_deref(),
            Some("https://github.com/apps/mai-test/installations/select_target?state=state-1")
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
}
