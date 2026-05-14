use mai_protocol::{
    GithubAppManifestStartRequest, GithubAppManifestStartResponse, GithubAppSettingsResponse,
    GithubInstallationsResponse, GithubRepositoriesResponse, GithubRepositorySummary,
    RelayGithubInstallationTokenResponse,
};
use mai_runtime::github::GithubAppBackend;
use mai_runtime::{RuntimeError, Result};

use crate::client::RelayClient;

#[async_trait::async_trait]
impl GithubAppBackend for RelayClient {
    async fn github_app_settings(&self) -> Result<GithubAppSettingsResponse> {
        RelayClient::github_app_settings(self).await
    }

    async fn save_github_app_settings(
        &self,
        _request: mai_protocol::GithubAppSettingsRequest,
    ) -> Result<GithubAppSettingsResponse> {
        self.github_app_settings().await
    }

    async fn start_github_app_manifest(
        &self,
        request: GithubAppManifestStartRequest,
    ) -> Result<GithubAppManifestStartResponse> {
        RelayClient::start_github_app_manifest(self, request).await
    }

    async fn complete_github_app_manifest(
        &self,
        _code: &str,
        _state: &str,
    ) -> Result<GithubAppSettingsResponse> {
        Err(RuntimeError::InvalidInput(
            "GitHub App manifest callback is handled by mai-relay".to_string(),
        ))
    }

    async fn list_github_installations(&self) -> Result<GithubInstallationsResponse> {
        RelayClient::list_github_installations(self).await
    }

    async fn refresh_github_installations(&self) -> Result<GithubInstallationsResponse> {
        RelayClient::list_github_installations(self).await
    }

    async fn list_github_repositories(
        &self,
        installation_id: u64,
    ) -> Result<GithubRepositoriesResponse> {
        RelayClient::list_github_repositories(self, installation_id).await
    }

    async fn get_github_repository(
        &self,
        installation_id: u64,
        repository_full_name: &str,
    ) -> Result<GithubRepositorySummary> {
        RelayClient::get_github_repository(self, installation_id, repository_full_name).await
    }

    async fn github_installation_token(
        &self,
        installation_id: u64,
        repository_id: Option<u64>,
        include_packages: bool,
    ) -> Result<RelayGithubInstallationTokenResponse> {
        RelayClient::create_installation_token(
            self,
            installation_id,
            repository_id,
            include_packages,
        )
        .await
    }
}
