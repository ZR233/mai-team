use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use mai_protocol::{
    GitTokenKind, GithubAppManifestAccountType, GithubAppManifestStartRequest,
    GithubAppManifestStartResponse, GithubAppSettingsRequest, GithubAppSettingsResponse,
    GithubInstallationSummary, GithubInstallationsResponse, GithubRepositoriesResponse,
    GithubRepositorySummary, RelayGithubInstallationTokenResponse, preview,
};
use mai_store::ConfigStore;
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::Instant;
use uuid::Uuid;

use crate::{Result, RuntimeError};

pub(crate) const DEFAULT_GITHUB_API_BASE_URL: &str = "https://api.github.com";
const DEFAULT_GITHUB_WEB_BASE_URL: &str = "https://github.com";
pub(crate) const GITHUB_HTTP_TIMEOUT_SECS: u64 = 10;
const GITHUB_API_VERSION: &str = "2022-11-28";
const GITHUB_TOKEN_REFRESH_SKEW_SECS: i64 = 120;
const GITHUB_MANIFEST_STATE_TTL_SECS: u64 = 900;

mod accounts;
mod packages;

pub(crate) use accounts::GitAccountService;
pub(crate) use packages::repository_packages_with_token;

#[async_trait]
pub trait GithubAppBackend: Send + Sync {
    async fn github_app_settings(&self) -> Result<GithubAppSettingsResponse>;
    async fn save_github_app_settings(
        &self,
        request: GithubAppSettingsRequest,
    ) -> Result<GithubAppSettingsResponse>;
    async fn start_github_app_manifest(
        &self,
        request: GithubAppManifestStartRequest,
    ) -> Result<GithubAppManifestStartResponse>;
    async fn complete_github_app_manifest(
        &self,
        code: &str,
        state: &str,
    ) -> Result<GithubAppSettingsResponse>;
    async fn list_github_installations(&self) -> Result<GithubInstallationsResponse>;
    async fn refresh_github_installations(&self) -> Result<GithubInstallationsResponse>;
    async fn list_github_repositories(
        &self,
        installation_id: u64,
    ) -> Result<GithubRepositoriesResponse>;
    async fn get_github_repository(
        &self,
        installation_id: u64,
        repository_full_name: &str,
    ) -> Result<GithubRepositorySummary>;
    async fn github_installation_token(
        &self,
        installation_id: u64,
        repository_id: Option<u64>,
        include_packages: bool,
    ) -> Result<RelayGithubInstallationTokenResponse>;
}

#[derive(Debug, Clone)]
struct CachedGithubToken {
    token: String,
    expires_at: DateTime<Utc>,
}

pub(crate) struct DirectGithubAppBackend {
    store: Arc<ConfigStore>,
    github_http: reqwest::Client,
    github_api_base_url: String,
    github_tokens: Mutex<HashMap<String, CachedGithubToken>>,
    github_manifest_states: Mutex<HashMap<String, GithubManifestState>>,
}

impl DirectGithubAppBackend {
    pub(crate) fn new(
        store: Arc<ConfigStore>,
        github_http: reqwest::Client,
        github_api_base_url: String,
    ) -> Self {
        Self {
            store,
            github_http,
            github_api_base_url,
            github_tokens: Mutex::new(HashMap::new()),
            github_manifest_states: Mutex::new(HashMap::new()),
        }
    }

    async fn github_app_secret(&self) -> Result<(String, String, String)> {
        self.store.github_app_secret().await?.ok_or_else(|| {
            RuntimeError::InvalidInput(
                "GitHub App ID and private key must be configured before using Projects"
                    .to_string(),
            )
        })
    }

    async fn github_app_jwt(&self) -> Result<(String, String)> {
        let (app_id, private_key, base_url) = self.github_app_secret().await?;
        let now = Utc::now().timestamp();
        let claims = GithubJwtClaims {
            iat: now.saturating_sub(60) as usize,
            exp: now.saturating_add(540) as usize,
            iss: app_id,
        };
        let token = encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &EncodingKey::from_rsa_pem(private_key.as_bytes())?,
        )?;
        Ok((token, base_url))
    }

    async fn prune_github_manifest_states(&self) {
        let ttl = std::time::Duration::from_secs(GITHUB_MANIFEST_STATE_TTL_SECS);
        let mut states = self.github_manifest_states.lock().await;
        states.retain(|_, state| state.created_at.elapsed() < ttl);
    }

    async fn take_github_manifest_state(&self, state: &str) -> Result<GithubManifestState> {
        self.prune_github_manifest_states().await;
        let record = self
            .github_manifest_states
            .lock()
            .await
            .remove(state)
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "GitHub App setup link expired or state is invalid. Start configuration again."
                        .to_string(),
                )
            })?;
        Ok(record)
    }
}

#[async_trait]
impl GithubAppBackend for DirectGithubAppBackend {
    async fn github_app_settings(&self) -> Result<GithubAppSettingsResponse> {
        Ok(self.store.get_github_app_settings().await?)
    }

    async fn save_github_app_settings(
        &self,
        request: GithubAppSettingsRequest,
    ) -> Result<GithubAppSettingsResponse> {
        self.github_tokens.lock().await.clear();
        Ok(self.store.save_github_app_settings(request).await?)
    }

    async fn start_github_app_manifest(
        &self,
        request: GithubAppManifestStartRequest,
    ) -> Result<GithubAppManifestStartResponse> {
        let origin = sanitize_origin(&request.origin)?;
        let org = match request.account_type {
            GithubAppManifestAccountType::Organization => {
                let org = request
                    .org
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput("organization is required".to_string())
                    })?;
                if !is_valid_github_slug(org) {
                    return Err(RuntimeError::InvalidInput(
                        "organization may contain only letters, numbers, or hyphens".to_string(),
                    ));
                }
                Some(org.to_string())
            }
            GithubAppManifestAccountType::Personal => None,
        };
        let state = Uuid::new_v4().to_string();
        let urls = github_app_manifest_urls(&origin, &request.account_type, org.as_deref(), &state);
        let manifest = github_app_manifest(&urls.redirect_url, &urls.setup_url, &urls.webhook_url);

        self.prune_github_manifest_states().await;
        self.github_manifest_states.lock().await.insert(
            state.clone(),
            GithubManifestState {
                created_at: Instant::now(),
                account_type: request.account_type,
                org,
            },
        );
        Ok(GithubAppManifestStartResponse {
            state,
            action_url: urls.action_url,
            manifest,
        })
    }

    async fn complete_github_app_manifest(
        &self,
        code: &str,
        state: &str,
    ) -> Result<GithubAppSettingsResponse> {
        if !is_valid_github_manifest_code(code) {
            return Err(RuntimeError::InvalidInput(
                "invalid GitHub manifest code".to_string(),
            ));
        }
        let state_record = self.take_github_manifest_state(state).await?;
        let url = github_api_url(
            DEFAULT_GITHUB_API_BASE_URL,
            &format!("/app-manifests/{code}/conversions"),
        );
        let response = self
            .github_http
            .post(url)
            .headers(github_headers())
            .send()
            .await?;
        let conversion: GithubManifestConversionResponse =
            decode_github_response(response, "create app from manifest").await?;
        let owner_login = github_manifest_owner_login(&conversion, &state_record);
        let owner_type = github_manifest_owner_type(&conversion, &state_record);
        self.save_github_app_settings(GithubAppSettingsRequest {
            app_id: Some(conversion.id.to_string()),
            private_key: Some(conversion.pem),
            base_url: Some(DEFAULT_GITHUB_API_BASE_URL.to_string()),
            app_slug: Some(conversion.slug),
            app_html_url: Some(conversion.html_url),
            owner_login,
            owner_type,
        })
        .await
    }

    async fn list_github_installations(&self) -> Result<GithubInstallationsResponse> {
        let (jwt, base_url) = self.github_app_jwt().await?;
        let url = github_api_url(&base_url, "/app/installations?per_page=100");
        let response = self
            .github_http
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
                .map(github_installation_summary)
                .collect(),
        })
    }

    async fn refresh_github_installations(&self) -> Result<GithubInstallationsResponse> {
        self.github_tokens.lock().await.clear();
        self.list_github_installations().await
    }

    async fn list_github_repositories(
        &self,
        installation_id: u64,
    ) -> Result<GithubRepositoriesResponse> {
        if installation_id == 0 {
            return Err(RuntimeError::InvalidInput(
                "installation_id is required".to_string(),
            ));
        }
        let token = self
            .github_installation_token(installation_id, None, false)
            .await?
            .token;
        let (_, _, base_url) = self.github_app_secret().await?;
        let url = github_api_url(&base_url, "/installation/repositories?per_page=100");
        let response = self
            .github_http
            .get(url)
            .bearer_auth(token)
            .headers(github_headers())
            .send()
            .await?;
        let response: GithubRepositoriesApi =
            decode_github_response(response, "list installation repositories").await?;
        Ok(GithubRepositoriesResponse {
            repositories: response
                .repositories
                .into_iter()
                .map(github_repository_summary)
                .collect(),
        })
    }

    async fn get_github_repository(
        &self,
        installation_id: u64,
        repository_full_name: &str,
    ) -> Result<GithubRepositorySummary> {
        let token = self
            .github_installation_token(installation_id, None, false)
            .await?
            .token;
        let url = github_api_url(
            &self.github_api_base_url,
            &format!("/repos/{repository_full_name}"),
        );
        let response = self
            .github_http
            .get(url)
            .bearer_auth(token)
            .headers(github_headers())
            .send()
            .await?;
        let repository: GithubRepositoryApi =
            decode_github_response(response, "get repository").await?;
        Ok(github_repository_summary(repository))
    }

    async fn github_installation_token(
        &self,
        installation_id: u64,
        repository_id: Option<u64>,
        include_packages: bool,
    ) -> Result<RelayGithubInstallationTokenResponse> {
        let cache_key =
            github_installation_token_cache_key(installation_id, repository_id, include_packages);
        {
            let tokens = self.github_tokens.lock().await;
            if let Some(cached) = tokens.get(&cache_key)
                && cached.expires_at - TimeDelta::seconds(GITHUB_TOKEN_REFRESH_SKEW_SECS)
                    > Utc::now()
            {
                return Ok(RelayGithubInstallationTokenResponse {
                    token: cached.token.clone(),
                    expires_at: cached.expires_at,
                });
            }
        }

        let (jwt, base_url) = self.github_app_jwt().await?;
        let url = github_api_url(
            &base_url,
            &format!("/app/installations/{installation_id}/access_tokens"),
        );
        let body = github_access_token_request(repository_id, include_packages);
        let response = self
            .github_http
            .post(url)
            .bearer_auth(jwt)
            .headers(github_headers())
            .json(&body)
            .send()
            .await?;
        let token: GithubAccessTokenResponse =
            decode_github_response(response, "create installation token").await?;
        self.github_tokens.lock().await.insert(
            cache_key,
            CachedGithubToken {
                token: token.token.clone(),
                expires_at: token.expires_at,
            },
        );
        Ok(RelayGithubInstallationTokenResponse {
            token: token.token,
            expires_at: token.expires_at,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GithubManifestState {
    pub(crate) created_at: Instant,
    pub(crate) account_type: GithubAppManifestAccountType,
    pub(crate) org: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubAppManifestUrls {
    pub(crate) redirect_url: String,
    pub(crate) setup_url: String,
    pub(crate) webhook_url: String,
    pub(crate) action_url: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct GithubJwtClaims {
    pub(crate) iat: usize,
    pub(crate) exp: usize,
    pub(crate) iss: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubAccountApi {
    pub(crate) login: String,
    #[serde(rename = "type")]
    pub(crate) account_type: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubUserApi {
    pub(crate) login: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubInstallationApi {
    pub(crate) id: u64,
    pub(crate) account: GithubAccountApi,
    pub(crate) repository_selection: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubRepositoriesApi {
    pub(crate) repositories: Vec<GithubRepositoryApi>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubRepositoryApi {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) full_name: String,
    pub(crate) private: bool,
    pub(crate) clone_url: String,
    pub(crate) html_url: String,
    pub(crate) default_branch: Option<String>,
    pub(crate) owner: GithubAccountApi,
}

#[derive(Debug, Serialize)]
pub(crate) struct GithubAccessTokenRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) repository_ids: Option<Vec<u64>>,
    pub(crate) permissions: GithubAccessTokenPermissions,
}

#[derive(Debug, Serialize)]
pub(crate) struct GithubAccessTokenPermissions {
    pub(crate) contents: &'static str,
    pub(crate) pull_requests: &'static str,
    pub(crate) issues: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) packages: Option<&'static str>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubAccessTokenResponse {
    pub(crate) token: String,
    pub(crate) expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubManifestConversionResponse {
    pub(crate) id: u64,
    pub(crate) slug: String,
    pub(crate) html_url: String,
    pub(crate) pem: String,
    #[serde(default)]
    pub(crate) owner: Option<GithubAccountApi>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubErrorResponse {
    pub(crate) message: Option<String>,
}

pub(crate) fn github_clone_url(owner: &str, repo: &str) -> String {
    format!("https://github.com/{owner}/{repo}.git")
}

pub(crate) fn github_repository_summary(
    repository: GithubRepositoryApi,
) -> GithubRepositorySummary {
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

pub(crate) fn github_installation_summary(
    installation: GithubInstallationApi,
) -> GithubInstallationSummary {
    GithubInstallationSummary {
        id: installation.id,
        account_login: installation.account.login,
        account_type: installation.account.account_type,
        repository_selection: installation.repository_selection,
    }
}

pub(crate) fn github_api_url(base_url: &str, path: &str) -> String {
    let base = base_url
        .trim()
        .trim_end_matches('/')
        .if_empty(DEFAULT_GITHUB_API_BASE_URL);
    format!("{base}{path}")
}

pub(crate) fn normalize_github_api_get_path(path: &str) -> Result<String> {
    let path = path.trim();
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('#')
        || path.contains(char::is_whitespace)
    {
        return Err(RuntimeError::InvalidInput(
            "github_api_get path must be a GitHub API path beginning with `/`".to_string(),
        ));
    }
    Ok(path.to_string())
}

pub(crate) fn github_path_segment(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

pub(crate) fn sanitize_origin(origin: &str) -> Result<String> {
    let origin = origin.trim().trim_end_matches('/');
    if origin.is_empty() {
        return Err(RuntimeError::InvalidInput("origin is required".to_string()));
    }
    if !(origin.starts_with("http://") || origin.starts_with("https://")) {
        return Err(RuntimeError::InvalidInput(
            "origin must start with http:// or https://".to_string(),
        ));
    }
    if origin.contains('#') || origin.contains('?') || origin.contains(char::is_whitespace) {
        return Err(RuntimeError::InvalidInput(
            "origin must be a plain browser origin".to_string(),
        ));
    }
    Ok(origin.to_string())
}

pub(crate) fn is_valid_github_manifest_code(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub(crate) fn is_valid_github_slug(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.len() <= 100
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub(crate) fn github_app_manifest_urls(
    origin: &str,
    account_type: &GithubAppManifestAccountType,
    org: Option<&str>,
    state: &str,
) -> GithubAppManifestUrls {
    let redirect_url = format!("{origin}/github/app-manifest/callback");
    let setup_url = format!("{origin}/github/app-installation/callback");
    let webhook_url = format!("{origin}/github/webhook-disabled");
    let action_url = match (account_type, org) {
        (GithubAppManifestAccountType::Organization, Some(org)) => {
            format!(
                "{DEFAULT_GITHUB_WEB_BASE_URL}/organizations/{org}/settings/apps/new?state={state}"
            )
        }
        _ => format!("{DEFAULT_GITHUB_WEB_BASE_URL}/settings/apps/new?state={state}"),
    };
    GithubAppManifestUrls {
        redirect_url,
        setup_url,
        webhook_url,
        action_url,
    }
}

pub(crate) fn github_app_manifest(redirect_url: &str, setup_url: &str, webhook_url: &str) -> Value {
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
            "issues": "write"
        },
        "default_events": [],
        "hook_attributes": {
            "url": webhook_url,
            "active": false
        }
    })
}

pub(crate) fn github_manifest_owner_login(
    conversion: &GithubManifestConversionResponse,
    state: &GithubManifestState,
) -> Option<String> {
    conversion
        .owner
        .as_ref()
        .map(|owner| owner.login.clone())
        .or_else(|| {
            state
                .org
                .clone()
                .filter(|_| state.account_type == GithubAppManifestAccountType::Organization)
        })
}

pub(crate) fn github_manifest_owner_type(
    conversion: &GithubManifestConversionResponse,
    state: &GithubManifestState,
) -> Option<String> {
    conversion
        .owner
        .as_ref()
        .map(|owner| owner.account_type.clone())
        .or_else(|| match state.account_type {
            GithubAppManifestAccountType::Organization => Some("Organization".to_string()),
            GithubAppManifestAccountType::Personal => Some("User".to_string()),
        })
}

pub(crate) fn github_installation_token_cache_key(
    installation_id: u64,
    repository_id: Option<u64>,
    include_packages: bool,
) -> String {
    format!(
        "{installation_id}:{}:{}",
        if include_packages {
            "packages"
        } else {
            "default"
        },
        repository_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "all".to_string())
    )
}

pub(crate) fn github_access_token_request(
    repository_id: Option<u64>,
    include_packages: bool,
) -> GithubAccessTokenRequest {
    GithubAccessTokenRequest {
        repository_ids: repository_id.map(|id| vec![id]),
        permissions: GithubAccessTokenPermissions {
            contents: "write",
            pull_requests: "write",
            issues: "write",
            packages: include_packages.then_some("read"),
        },
    }
}

pub(crate) fn github_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(USER_AGENT, HeaderValue::from_static("mai-team"));
    headers.insert(
        "X-GitHub-Api-Version",
        HeaderValue::from_static(GITHUB_API_VERSION),
    );
    headers
}

pub(crate) fn github_scopes(headers: &HeaderMap) -> Vec<String> {
    headers
        .get("x-oauth-scopes")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn git_token_kind(token: &str, scopes: &[String]) -> GitTokenKind {
    if token.starts_with("github_pat_") {
        GitTokenKind::FineGrainedPat
    } else if token.starts_with("ghp_") || !scopes.is_empty() {
        GitTokenKind::Classic
    } else {
        GitTokenKind::Unknown
    }
}

pub(crate) async fn decode_github_response<T: DeserializeOwned>(
    response: reqwest::Response,
    action: &str,
) -> Result<T> {
    let status = response.status();
    if status.is_success() {
        return Ok(response.json::<T>().await?);
    }
    let text = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<GithubErrorResponse>(&text)
        .ok()
        .and_then(|error| error.message)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| preview(&text, 300));
    Err(RuntimeError::InvalidInput(format!(
        "GitHub {action} failed ({status}): {message}"
    )))
}

trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl IfEmpty for &str {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.to_string()
        } else {
            self.to_string()
        }
    }
}
