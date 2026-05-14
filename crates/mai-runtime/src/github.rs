use mai_protocol::{GitTokenKind, GithubInstallationSummary, GithubRepositorySummary, preview};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::{Result, RuntimeError};

pub(crate) const DEFAULT_GITHUB_API_BASE_URL: &str = "https://api.github.com";
pub(crate) const GITHUB_HTTP_TIMEOUT_SECS: u64 = 10;
const GITHUB_API_VERSION: &str = "2022-11-28";

mod accounts;
mod app;
mod packages;

pub(crate) use accounts::{GitAccountService, VerifiedGithubRepository};
pub(crate) use app::DirectGithubAppBackend;
pub use app::GithubAppBackend;
pub(crate) use packages::repository_packages_with_token;

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
