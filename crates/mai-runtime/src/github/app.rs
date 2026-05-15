use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use mai_protocol::{
    GithubAppManifestAccountType, GithubAppManifestStartRequest, GithubAppManifestStartResponse,
    GithubAppSettingsRequest, GithubAppSettingsResponse, GithubInstallationsResponse,
    GithubRepositoriesResponse, GithubRepositorySummary, RelayGithubInstallationTokenResponse,
};
use mai_store::ConfigStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::Instant;
use uuid::Uuid;

use crate::{Result, RuntimeError};

use super::{
    DEFAULT_GITHUB_API_BASE_URL, GithubAccountApi, GithubInstallationApi, GithubRepositoriesApi,
    GithubRepositoryApi, decode_github_response, github_api_url, github_headers,
    github_installation_summary, github_repository_summary,
};

const DEFAULT_GITHUB_WEB_BASE_URL: &str = "https://github.com";
const GITHUB_TOKEN_REFRESH_SKEW_SECS: i64 = 120;
const GITHUB_MANIFEST_STATE_TTL_SECS: u64 = 900;

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

pub struct DirectGithubAppBackend {
    store: Arc<ConfigStore>,
    github_http: reqwest::Client,
    github_api_base_url: String,
    github_tokens: Mutex<HashMap<String, CachedGithubToken>>,
    github_manifest_states: Mutex<HashMap<String, GithubManifestState>>,
}

impl DirectGithubAppBackend {
    pub fn new(
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
            public_url: None,
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
struct GithubManifestState {
    created_at: Instant,
    account_type: GithubAppManifestAccountType,
    org: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GithubAppManifestUrls {
    redirect_url: String,
    setup_url: String,
    webhook_url: String,
    action_url: String,
}

#[derive(Debug, Serialize)]
struct GithubJwtClaims {
    iat: usize,
    exp: usize,
    iss: String,
}

#[derive(Debug, Serialize)]
struct GithubAccessTokenRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_ids: Option<Vec<u64>>,
    permissions: GithubAccessTokenPermissions,
}

#[derive(Debug, Serialize)]
struct GithubAccessTokenPermissions {
    contents: &'static str,
    pull_requests: &'static str,
    issues: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    packages: Option<&'static str>,
}

#[derive(Debug, Deserialize)]
struct GithubAccessTokenResponse {
    token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct GithubManifestConversionResponse {
    id: u64,
    slug: String,
    html_url: String,
    pem: String,
    #[serde(default)]
    owner: Option<GithubAccountApi>,
}

fn sanitize_origin(origin: &str) -> Result<String> {
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

fn is_valid_github_manifest_code(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn is_valid_github_slug(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.len() <= 100
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn github_app_manifest_urls(
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

fn github_app_manifest(redirect_url: &str, setup_url: &str, webhook_url: &str) -> Value {
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
            "issues": "write"
        },
        "default_events": [],
        "hook_attributes": {
            "url": webhook_url,
            "active": false
        }
    })
}

fn github_manifest_owner_login(
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

fn github_manifest_owner_type(
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

fn github_installation_token_cache_key(
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

fn github_access_token_request(
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn test_store(dir: &tempfile::TempDir) -> Arc<ConfigStore> {
        Arc::new(
            ConfigStore::open_with_config_and_artifact_index_path(
                dir.path().join("runtime.sqlite3"),
                dir.path().join("config.toml"),
                dir.path().join("data/artifacts/index"),
            )
            .await
            .expect("open store"),
        )
    }

    async fn test_backend(dir: &tempfile::TempDir) -> DirectGithubAppBackend {
        DirectGithubAppBackend::new(
            test_store(dir).await,
            reqwest::Client::new(),
            DEFAULT_GITHUB_API_BASE_URL.to_string(),
        )
    }

    #[tokio::test]
    async fn manifest_start_builds_org_action_and_manifest() {
        let dir = tempdir().expect("tempdir");
        let backend = test_backend(&dir).await;

        let response = backend
            .start_github_app_manifest(GithubAppManifestStartRequest {
                origin: "http://127.0.0.1:8080/".to_string(),
                account_type: GithubAppManifestAccountType::Organization,
                org: Some("mai-org".to_string()),
            })
            .await
            .expect("start manifest");

        assert!(
            response
                .action_url
                .starts_with("https://github.com/organizations/mai-org/settings/apps/new?state=")
        );
        assert!(response.action_url.ends_with(&response.state));
        assert_eq!(
            response.manifest["redirect_url"],
            "http://127.0.0.1:8080/github/app-manifest/callback"
        );
        assert_eq!(
            response.manifest["default_permissions"]["contents"],
            "write"
        );
        assert_eq!(
            response.manifest["default_permissions"]["pull_requests"],
            "write"
        );
        assert_eq!(response.manifest["default_permissions"]["issues"], "write");
        assert_eq!(response.manifest["public"], true);
        assert_eq!(response.manifest["hook_attributes"]["active"], false);
    }

    #[tokio::test]
    async fn manifest_start_rejects_invalid_org() {
        let dir = tempdir().expect("tempdir");
        let backend = test_backend(&dir).await;

        let result = backend
            .start_github_app_manifest(GithubAppManifestStartRequest {
                origin: "http://127.0.0.1:8080".to_string(),
                account_type: GithubAppManifestAccountType::Organization,
                org: Some("-bad-".to_string()),
            })
            .await;

        assert!(matches!(result, Err(RuntimeError::InvalidInput(_))));
    }
}
