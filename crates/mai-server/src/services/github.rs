use std::sync::Arc;

use mai_protocol::*;
use mai_relay_client::RelayClient;
use mai_runtime::{AgentRuntime, RuntimeError};

pub(crate) struct GithubService {
    runtime: Arc<AgentRuntime>,
    relay: Option<Arc<RelayClient>>,
}

impl GithubService {
    pub(crate) fn new(runtime: Arc<AgentRuntime>, relay: Option<Arc<RelayClient>>) -> Self {
        Self { runtime, relay }
    }

    pub(crate) async fn app_settings(&self) -> Result<GithubAppSettingsResponse, RuntimeError> {
        match &self.relay {
            Some(relay) => relay.github_app_settings().await,
            None => self.runtime.github_app_settings().await,
        }
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
        match &self.relay {
            Some(relay) => relay.start_github_app_manifest(request).await,
            None => self.runtime.start_github_app_manifest(request).await,
        }
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
        let relay = self.relay.as_ref().ok_or_else(|| {
            RuntimeError::InvalidInput("GitHub App installation requires relay mode".into())
        })?;
        relay.start_github_app_installation(request).await
    }

    pub(crate) async fn list_installations(
        &self,
    ) -> Result<GithubInstallationsResponse, RuntimeError> {
        match &self.relay {
            Some(relay) => relay.list_github_installations().await,
            None => self.runtime.list_github_installations().await,
        }
    }

    pub(crate) async fn refresh_installations(
        &self,
    ) -> Result<GithubInstallationsResponse, RuntimeError> {
        match &self.relay {
            Some(relay) => relay.list_github_installations().await,
            None => self.runtime.refresh_github_installations().await,
        }
    }

    pub(crate) async fn list_repositories(
        &self,
        installation_id: u64,
    ) -> Result<GithubRepositoriesResponse, RuntimeError> {
        match &self.relay {
            Some(relay) => relay.list_github_repositories(installation_id).await,
            None => self.runtime.list_github_repositories(installation_id).await,
        }
    }

    pub(crate) async fn list_repository_packages(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
    ) -> Result<RepositoryPackagesResponse, RuntimeError> {
        match &self.relay {
            Some(relay) => {
                let request = GithubAppInstallationPackagesRequest {
                    installation_id,
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                };
                relay.list_github_repository_packages(request).await
            }
            None => {
                self.runtime
                    .list_github_installation_repository_packages(installation_id, owner, repo)
                    .await
            }
        }
    }

    pub(crate) async fn relay_status(&self) -> RelayStatusResponse {
        match &self.relay {
            Some(relay) => relay.status().await,
            None => RelayStatusResponse {
                enabled: false,
                connected: false,
                relay_url: None,
                node_id: None,
                last_heartbeat_at: None,
                queued_deliveries: None,
                message: Some("relay disabled".to_string()),
            },
        }
    }
}
