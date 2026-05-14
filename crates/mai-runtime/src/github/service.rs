use std::sync::Arc;

use mai_protocol::{
    GitAccountRequest, GitAccountResponse, GitAccountSummary, GitAccountsResponse,
    GithubAppManifestStartRequest, GithubAppManifestStartResponse, GithubAppSettingsRequest,
    GithubAppSettingsResponse, GithubInstallationsResponse, GithubRepositoriesResponse,
    RepositoryPackagesResponse, preview,
};
use serde_json::{Value, json};

use super::{
    GitAccountService, GithubAppBackend, GithubErrorResponse, github_api_url, github_headers,
    normalize_github_api_get_path, repository_packages_with_token,
};
use crate::{Result, RuntimeError, turn::tools::ToolExecution};

pub(crate) async fn list_git_accounts(
    git_accounts: &GitAccountService,
) -> Result<GitAccountsResponse> {
    git_accounts.list().await
}

pub(crate) async fn save_git_account(
    git_accounts: &Arc<GitAccountService>,
    request: GitAccountRequest,
) -> Result<GitAccountResponse> {
    git_accounts.save(request).await
}

pub(crate) async fn verify_git_account(
    git_accounts: &GitAccountService,
    account_id: &str,
) -> Result<GitAccountSummary> {
    git_accounts.verify(account_id).await
}

pub(crate) async fn delete_git_account(
    git_accounts: &GitAccountService,
    account_id: &str,
) -> Result<GitAccountsResponse> {
    git_accounts.delete(account_id).await
}

pub(crate) async fn set_default_git_account(
    git_accounts: &GitAccountService,
    account_id: &str,
) -> Result<GitAccountsResponse> {
    git_accounts.set_default(account_id).await
}

pub(crate) async fn list_git_account_repositories(
    git_accounts: &GitAccountService,
    account_id: &str,
) -> Result<GithubRepositoriesResponse> {
    git_accounts.list_repositories(account_id).await
}

pub(crate) async fn list_git_account_repository_packages(
    git_accounts: &GitAccountService,
    account_id: &str,
    owner: &str,
    repo: &str,
) -> Result<RepositoryPackagesResponse> {
    git_accounts
        .list_repository_packages(account_id, owner, repo)
        .await
}

pub(crate) async fn list_github_installation_repository_packages(
    github_backend: &dyn GithubAppBackend,
    github_http: &reqwest::Client,
    github_api_base_url: &str,
    installation_id: u64,
    owner: &str,
    repo: &str,
) -> Result<RepositoryPackagesResponse> {
    let token = github_backend
        .github_installation_token(installation_id, None, true)
        .await?
        .token;
    repository_packages_with_token(github_http, github_api_base_url, &token, owner, repo).await
}

pub(crate) async fn github_app_settings(
    github_backend: &dyn GithubAppBackend,
) -> Result<GithubAppSettingsResponse> {
    github_backend.github_app_settings().await
}

pub(crate) async fn save_github_app_settings(
    github_backend: &dyn GithubAppBackend,
    request: GithubAppSettingsRequest,
) -> Result<GithubAppSettingsResponse> {
    github_backend.save_github_app_settings(request).await
}

pub(crate) async fn start_github_app_manifest(
    github_backend: &dyn GithubAppBackend,
    request: GithubAppManifestStartRequest,
) -> Result<GithubAppManifestStartResponse> {
    github_backend.start_github_app_manifest(request).await
}

pub(crate) async fn complete_github_app_manifest(
    github_backend: &dyn GithubAppBackend,
    code: &str,
    state: &str,
) -> Result<GithubAppSettingsResponse> {
    github_backend
        .complete_github_app_manifest(code, state)
        .await
}

pub(crate) async fn list_github_installations(
    github_backend: &dyn GithubAppBackend,
) -> Result<GithubInstallationsResponse> {
    github_backend.list_github_installations().await
}

pub(crate) async fn refresh_github_installations(
    github_backend: &dyn GithubAppBackend,
) -> Result<GithubInstallationsResponse> {
    github_backend.refresh_github_installations().await
}

pub(crate) async fn list_github_repositories(
    github_backend: &dyn GithubAppBackend,
    installation_id: u64,
) -> Result<GithubRepositoriesResponse> {
    github_backend
        .list_github_repositories(installation_id)
        .await
}

pub(crate) async fn execute_project_github_api_get(
    github_http: &reqwest::Client,
    github_api_base_url: &str,
    token: Option<String>,
    path: &str,
) -> Result<ToolExecution> {
    let Some(token) = token else {
        return Err(RuntimeError::InvalidInput(
            "agent is not attached to a project".to_string(),
        ));
    };
    let path = normalize_github_api_get_path(path)?;
    let url = github_api_url(github_api_base_url, &path);
    let response = github_http
        .get(url)
        .bearer_auth(&token)
        .headers(github_headers())
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let output = if status.is_success() {
        serde_json::from_str::<Value>(&text)
            .unwrap_or_else(|_| json!({ "status": status.as_u16(), "body": text }))
    } else {
        let message = serde_json::from_str::<GithubErrorResponse>(&text)
            .ok()
            .and_then(|error| error.message)
            .filter(|message| !message.trim().is_empty())
            .unwrap_or_else(|| preview(&text, 300));
        json!({
            "status": status.as_u16(),
            "error": redact_secret(&message, &token),
        })
    };
    Ok(ToolExecution::new(
        status.is_success(),
        redact_secret(&output.to_string(), &token),
        false,
    ))
}

fn redact_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        return value.to_string();
    }
    value.replace(secret, "<redacted>")
}
