use std::sync::Arc;

use mai_protocol::*;
use mai_runtime::{AgentRuntime, RuntimeError};

use crate::services::relay_manager::{RelayManager, list_relay_repository_packages};

pub(crate) struct GithubService {
    runtime: Arc<AgentRuntime>,
    relay: Arc<RelayManager>,
}

impl GithubService {
    pub(crate) fn new(runtime: Arc<AgentRuntime>, relay: Arc<RelayManager>) -> Self {
        Self { runtime, relay }
    }

    pub(crate) async fn app_settings(&self) -> Result<GithubAppSettingsResponse, RuntimeError> {
        self.runtime.github_app_settings().await
    }

    pub(crate) async fn save_app_settings(
        &self,
        request: GithubAppSettingsRequest,
    ) -> Result<GithubAppSettingsResponse, RuntimeError> {
        self.runtime.save_github_app_settings(request).await
    }

    pub(crate) async fn start_manifest(
        &self,
        request: GithubAppManifestStartRequest,
    ) -> Result<GithubAppManifestStartResponse, RuntimeError> {
        self.runtime.start_github_app_manifest(request).await
    }

    pub(crate) async fn complete_manifest(
        &self,
        code: &str,
        state: &str,
    ) -> Result<GithubAppSettingsResponse, RuntimeError> {
        self.runtime.complete_github_app_manifest(code, state).await
    }

    pub(crate) async fn start_installation(
        &self,
        request: GithubAppInstallationStartRequest,
    ) -> Result<GithubAppInstallationStartResponse, RuntimeError> {
        let relay = self.relay.client().await.ok_or_else(|| {
            RuntimeError::InvalidInput("GitHub App installation requires relay mode".into())
        })?;
        relay.start_github_app_installation(request).await
    }

    pub(crate) async fn list_installations(
        &self,
    ) -> Result<GithubInstallationsResponse, RuntimeError> {
        self.runtime.list_github_installations().await
    }

    pub(crate) async fn refresh_installations(
        &self,
    ) -> Result<GithubInstallationsResponse, RuntimeError> {
        self.runtime.refresh_github_installations().await
    }

    pub(crate) async fn list_repositories(
        &self,
        installation_id: u64,
    ) -> Result<GithubRepositoriesResponse, RuntimeError> {
        self.runtime.list_github_repositories(installation_id).await
    }

    pub(crate) async fn list_repository_packages(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
    ) -> Result<RepositoryPackagesResponse, RuntimeError> {
        if let Some(relay) = self.relay.client().await {
            list_relay_repository_packages(relay, installation_id, owner, repo).await
        } else if self.relay.settings().await?.enabled {
            Err(RuntimeError::InvalidInput(
                "relay is enabled but not connected".to_string(),
            ))
        } else {
            self.runtime
                .list_github_installation_repository_packages(installation_id, owner, repo)
                .await
        }
    }

    pub(crate) async fn relay_status(&self) -> RelayStatusResponse {
        self.relay.status().await
    }

    pub(crate) async fn relay_settings(&self) -> Result<RelaySettingsResponse, RuntimeError> {
        Ok(self.relay.settings().await?)
    }

    pub(crate) async fn save_relay_settings(
        &self,
        request: RelaySettingsRequest,
    ) -> Result<RelaySettingsResponse, RuntimeError> {
        Ok(self.relay.save_settings(request).await?)
    }
}
