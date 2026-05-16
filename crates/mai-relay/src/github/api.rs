use crate::error::{RelayErrorKind, RelayResult};
use crate::state::AppState;
use chrono::{TimeDelta, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use mai_protocol::{
    GithubInstallationSummary, GithubInstallationsResponse, GithubRepositoriesResponse,
    GithubRepositorySummary, RelayGithubInstallationTokenRequest,
    RelayGithubInstallationTokenResponse,
};
use reqwest::header::{ACCEPT, HeaderValue, USER_AGENT};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::types::{
    GithubAccessTokenPermissions, GithubAccessTokenRequest, GithubAccessTokenResponse,
    GithubAppConfig, GithubErrorResponse, GithubInstallationApi, GithubJwtClaims, GithubPackageApi,
    GithubPackageVersionApi, GithubRepositoriesApi, GithubRepositoryApi,
};

const GITHUB_API_VERSION: &str = "2022-11-28";

pub(crate) async fn list_installations(
    state: &AppState,
) -> RelayResult<GithubInstallationsResponse> {
    let (jwt, base_url) = github_app_jwt(state)?;
    let url = github_api_url(&base_url, "/app/installations?per_page=100");
    let response = state
        .http
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
            .map(|installation| GithubInstallationSummary {
                id: installation.id,
                account_login: installation.account.login,
                account_type: installation.account.account_type,
                repository_selection: installation.repository_selection,
                events: installation.events,
            })
            .collect(),
    })
}

pub(crate) async fn list_repositories(
    state: &AppState,
    installation_id: u64,
) -> RelayResult<Value> {
    let token = create_installation_token(
        state,
        RelayGithubInstallationTokenRequest {
            installation_id,
            repository_id: None,
            include_packages: false,
        },
    )
    .await?;
    let url = github_api_url(
        &state.github_api_base_url,
        "/installation/repositories?per_page=100",
    );
    let response = state
        .http
        .get(url)
        .bearer_auth(token.token)
        .headers(github_headers())
        .send()
        .await?;
    let response: GithubRepositoriesApi =
        decode_github_response(response, "list installation repositories").await?;
    crate::rpc::to_value(GithubRepositoriesResponse {
        repositories: response
            .repositories
            .into_iter()
            .map(github_repository_summary)
            .collect(),
    })
}

pub(crate) async fn verify_installation(state: &AppState, installation_id: u64) -> RelayResult<()> {
    let installations = list_installations(state).await?;
    if installations
        .installations
        .iter()
        .any(|installation| installation.id == installation_id)
    {
        Ok(())
    } else {
        Err(RelayErrorKind::InvalidInput(format!(
            "GitHub App installation {installation_id} was not found"
        )))
    }
}

pub(crate) async fn get_repository(
    state: &AppState,
    request: mai_protocol::RelayGithubRepositoryGetRequest,
) -> RelayResult<Value> {
    let token = create_installation_token(
        state,
        RelayGithubInstallationTokenRequest {
            installation_id: request.installation_id,
            repository_id: None,
            include_packages: false,
        },
    )
    .await?;
    let path = format!("/repos/{}", request.repository_full_name);
    let url = github_api_url(&state.github_api_base_url, &path);
    let response = state
        .http
        .get(url)
        .bearer_auth(token.token)
        .headers(github_headers())
        .send()
        .await?;
    let repository: GithubRepositoryApi =
        decode_github_response(response, "get repository").await?;
    crate::rpc::to_value(github_repository_summary(repository))
}

pub(crate) async fn create_installation_token(
    state: &AppState,
    request: RelayGithubInstallationTokenRequest,
) -> RelayResult<RelayGithubInstallationTokenResponse> {
    if request.installation_id == 0 {
        return Err(RelayErrorKind::InvalidInput(
            "installation_id is required".to_string(),
        ));
    }
    if let Some(cached) = state.store.cached_token(
        request.installation_id,
        request.repository_id,
        request.include_packages,
    )? && cached.expires_at - TimeDelta::seconds(super::TOKEN_REFRESH_SKEW_SECS) > Utc::now()
    {
        return Ok(cached);
    }
    let (jwt, base_url) = github_app_jwt(state)?;
    let url = github_api_url(
        &base_url,
        &format!(
            "/app/installations/{}/access_tokens",
            request.installation_id
        ),
    );
    let body = GithubAccessTokenRequest {
        repository_ids: request.repository_id.map(|id| vec![id]),
        permissions: GithubAccessTokenPermissions {
            contents: "write",
            pull_requests: "write",
            issues: "write",
            checks: "read",
            statuses: "read",
            packages: request.include_packages.then_some("read"),
        },
    };
    let response = state
        .http
        .post(url)
        .bearer_auth(jwt)
        .headers(github_headers())
        .json(&body)
        .send()
        .await?;
    let token: GithubAccessTokenResponse =
        decode_github_response(response, "create installation token").await?;
    let token = RelayGithubInstallationTokenResponse {
        token: token.token,
        expires_at: token.expires_at,
    };
    state.store.save_cached_token(
        request.installation_id,
        request.repository_id,
        request.include_packages,
        &token,
    )?;
    Ok(token)
}

pub(crate) fn github_app_jwt(state: &AppState) -> RelayResult<(String, String)> {
    let config = state
        .store
        .github_app_config()?
        .ok_or_else(|| RelayErrorKind::InvalidInput("GitHub App is not configured".to_string()))?;
    let token = github_app_jwt_for_config(&config)?;
    Ok((token, state.github_api_base_url.clone()))
}

pub(crate) fn github_app_jwt_for_config(config: &GithubAppConfig) -> RelayResult<String> {
    let now = Utc::now().timestamp();
    let claims = GithubJwtClaims {
        iat: now.saturating_sub(60) as usize,
        exp: now.saturating_add(540) as usize,
        iss: config.app_id.clone(),
    };
    let token = encode(
        &Header::new(Algorithm::RS256),
        &claims,
        &EncodingKey::from_rsa_pem(config.private_key.as_bytes())?,
    )?;
    Ok(token)
}

pub(crate) async fn github_container_packages_for_owner(
    state: &AppState,
    token: &str,
    owner: &str,
) -> std::result::Result<Vec<GithubPackageApi>, reqwest::Error> {
    let org_url = github_api_url(
        &state.github_api_base_url,
        &format!(
            "/orgs/{}/packages?package_type=container&per_page=100",
            github_path_segment(owner)
        ),
    );
    let org_response = state
        .http
        .get(org_url)
        .bearer_auth(token)
        .headers(github_headers())
        .send()
        .await?;
    if org_response.status() != reqwest::StatusCode::NOT_FOUND {
        return org_response.error_for_status()?.json().await;
    }
    let user_url = github_api_url(
        &state.github_api_base_url,
        &format!(
            "/users/{}/packages?package_type=container&per_page=100",
            github_path_segment(owner)
        ),
    );
    state
        .http
        .get(user_url)
        .bearer_auth(token)
        .headers(github_headers())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
}

pub(crate) async fn github_container_package_versions(
    state: &AppState,
    token: &str,
    owner: &str,
    package_name: &str,
) -> std::result::Result<Vec<GithubPackageVersionApi>, reqwest::Error> {
    let org_url = github_api_url(
        &state.github_api_base_url,
        &format!(
            "/orgs/{}/packages/container/{}/versions?per_page=30",
            github_path_segment(owner),
            github_path_segment(package_name)
        ),
    );
    let org_response = state
        .http
        .get(org_url)
        .bearer_auth(token)
        .headers(github_headers())
        .send()
        .await?;
    if org_response.status() != reqwest::StatusCode::NOT_FOUND {
        return org_response.error_for_status()?.json().await;
    }
    let user_url = github_api_url(
        &state.github_api_base_url,
        &format!(
            "/users/{}/packages/container/{}/versions?per_page=30",
            github_path_segment(owner),
            github_path_segment(package_name)
        ),
    );
    state
        .http
        .get(user_url)
        .bearer_auth(token)
        .headers(github_headers())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
}

pub(crate) fn github_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(USER_AGENT, HeaderValue::from_static("mai-team-relay"));
    headers.insert(
        "X-GitHub-Api-Version",
        HeaderValue::from_static(GITHUB_API_VERSION),
    );
    headers
}

pub(crate) async fn decode_github_response<T>(
    response: reqwest::Response,
    action: &str,
) -> RelayResult<T>
where
    T: DeserializeOwned,
{
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if status.is_success() {
        return Ok(serde_json::from_str(&text)?);
    }
    let message = serde_json::from_str::<GithubErrorResponse>(&text)
        .ok()
        .and_then(|error| error.message)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| text.chars().take(300).collect());
    Err(RelayErrorKind::InvalidInput(format!(
        "{action} failed with {status}: {message}"
    )))
}

pub(crate) fn github_api_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
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
pub(crate) fn github_path_segment(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}
