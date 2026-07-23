use std::sync::Arc;

use mai_protocol::{
    GitAccountRequest, GitAccountResponse, GitAccountSummary, GitAccountsResponse,
    GithubAppManifestStartRequest, GithubAppManifestStartResponse, GithubAppSettingsRequest,
    GithubAppSettingsResponse, GithubInstallationsResponse, GithubRepositoriesResponse,
    RepositoryPackagesResponse,
};

use crate::{AgentRuntime, Result, github};

impl AgentRuntime {
    pub async fn list_git_accounts(&self) -> Result<GitAccountsResponse> {
        github::list_git_accounts(&self.deps.git_accounts).await
    }

    pub async fn save_git_account(
        self: &Arc<Self>,
        request: GitAccountRequest,
    ) -> Result<GitAccountResponse> {
        github::save_git_account(&self.deps.git_accounts, request).await
    }

    pub async fn verify_git_account(&self, account_id: &str) -> Result<GitAccountSummary> {
        github::verify_git_account(&self.deps.git_accounts, account_id).await
    }

    pub async fn delete_git_account(&self, account_id: &str) -> Result<GitAccountsResponse> {
        github::delete_git_account(&self.deps.git_accounts, account_id).await
    }

    pub async fn set_default_git_account(&self, account_id: &str) -> Result<GitAccountsResponse> {
        github::set_default_git_account(&self.deps.git_accounts, account_id).await
    }

    pub async fn list_git_account_repositories(
        &self,
        account_id: &str,
    ) -> Result<GithubRepositoriesResponse> {
        github::list_git_account_repositories(&self.deps.git_accounts, account_id).await
    }

    pub async fn list_git_account_repository_packages(
        &self,
        account_id: &str,
        owner: &str,
        repo: &str,
    ) -> Result<RepositoryPackagesResponse> {
        github::list_git_account_repository_packages(
            &self.deps.git_accounts,
            account_id,
            owner,
            repo,
        )
        .await
    }

    pub async fn list_github_installation_repository_packages(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
    ) -> Result<RepositoryPackagesResponse> {
        github::list_github_installation_repository_packages(
            self.deps.github_backend.as_ref(),
            &self.deps.github_http,
            &self.github_api_base_url,
            installation_id,
            owner,
            repo,
        )
        .await
    }

    pub async fn github_app_settings(&self) -> Result<GithubAppSettingsResponse> {
        github::github_app_settings(self.deps.github_backend.as_ref()).await
    }

    pub async fn refresh_github_app_settings(&self) -> Result<GithubAppSettingsResponse> {
        github::refresh_github_app_settings(self.deps.github_backend.as_ref()).await
    }

    pub async fn save_github_app_settings(
        &self,
        request: GithubAppSettingsRequest,
    ) -> Result<GithubAppSettingsResponse> {
        github::save_github_app_settings(self.deps.github_backend.as_ref(), request).await
    }

    pub async fn start_github_app_manifest(
        &self,
        request: GithubAppManifestStartRequest,
    ) -> Result<GithubAppManifestStartResponse> {
        github::start_github_app_manifest(self.deps.github_backend.as_ref(), request).await
    }

    pub async fn complete_github_app_manifest(
        &self,
        code: &str,
        state: &str,
    ) -> Result<GithubAppSettingsResponse> {
        github::complete_github_app_manifest(self.deps.github_backend.as_ref(), code, state).await
    }

    pub async fn list_github_installations(&self) -> Result<GithubInstallationsResponse> {
        github::list_github_installations(self.deps.github_backend.as_ref()).await
    }

    pub async fn refresh_github_installations(&self) -> Result<GithubInstallationsResponse> {
        github::refresh_github_installations(self.deps.github_backend.as_ref()).await
    }

    pub async fn list_github_repositories(
        &self,
        installation_id: u64,
    ) -> Result<GithubRepositoriesResponse> {
        github::list_github_repositories(self.deps.github_backend.as_ref(), installation_id).await
    }
}
