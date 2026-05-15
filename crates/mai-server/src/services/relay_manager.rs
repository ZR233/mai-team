use std::sync::Arc;

use mai_protocol::{
    GithubAppInstallationPackagesRequest, GithubAppManifestStartRequest,
    GithubAppManifestStartResponse, GithubAppSettingsRequest, GithubAppSettingsResponse,
    GithubInstallationsResponse, GithubRepositoriesResponse, GithubRepositorySummary,
    RelayGithubInstallationTokenResponse, RelaySettingsRequest, RelaySettingsResponse,
    RelayStatusResponse,
};
use mai_relay_client::{RelayClient, RelayClientConfig};
use mai_runtime::github::GithubAppBackend;
use mai_runtime::{AgentRuntime, RuntimeError};
use tokio::sync::RwLock;

use crate::services::relay_events;

pub(crate) struct RelayManager {
    store: Arc<mai_store::ConfigStore>,
    runtime: Arc<RwLock<Option<Arc<AgentRuntime>>>>,
    relay: RwLock<Option<Arc<RelayClient>>>,
}

impl RelayManager {
    pub(crate) fn new(store: Arc<mai_store::ConfigStore>) -> Arc<Self> {
        Arc::new(Self {
            store,
            runtime: Arc::new(RwLock::new(None)),
            relay: RwLock::new(None),
        })
    }

    pub(crate) async fn set_runtime(&self, runtime: Arc<AgentRuntime>) {
        *self.runtime.write().await = Some(runtime);
    }

    pub(crate) async fn configure_from_store(self: &Arc<Self>) -> mai_store::Result<()> {
        let Some((url, token, node_id)) = self.store.relay_secret().await? else {
            self.stop().await;
            return Ok(());
        };
        self.start(RelayClientConfig {
            url,
            token,
            node_id,
        })
        .await;
        Ok(())
    }

    pub(crate) async fn settings(&self) -> mai_store::Result<RelaySettingsResponse> {
        self.store.relay_settings().await
    }

    pub(crate) async fn save_settings(
        self: &Arc<Self>,
        request: RelaySettingsRequest,
    ) -> mai_store::Result<RelaySettingsResponse> {
        let saved = self.store.save_relay_settings(request).await?;
        self.configure_from_store().await?;
        Ok(saved)
    }

    pub(crate) async fn status(&self) -> RelayStatusResponse {
        match self.relay.read().await.clone() {
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

    pub(crate) async fn client(&self) -> Option<Arc<RelayClient>> {
        self.relay.read().await.clone()
    }

    async fn start(self: &Arc<Self>, config: RelayClientConfig) {
        self.stop().await;
        let relay = Arc::new(RelayClient::new(config));
        if let Some(runtime) = self.runtime.read().await.clone() {
            relay_events::install_relay_event_handler(Arc::clone(&relay), runtime).await;
        }
        Arc::clone(&relay).start();
        *self.relay.write().await = Some(relay);
    }

    async fn stop(&self) {
        if let Some(relay) = self.relay.write().await.take() {
            relay.stop();
        }
    }
}

#[derive(Clone)]
pub(crate) struct DynamicGithubAppBackend {
    direct: Arc<dyn GithubAppBackend>,
    relay: Arc<RelayManager>,
}

impl DynamicGithubAppBackend {
    pub(crate) fn new(direct: Arc<dyn GithubAppBackend>, relay: Arc<RelayManager>) -> Self {
        Self { direct, relay }
    }

    async fn backend(&self) -> Result<ActiveGithubAppBackend, RuntimeError> {
        if let Some(client) = self.relay.client().await {
            return Ok(ActiveGithubAppBackend::Relay(client));
        }
        let settings = self.relay.settings().await?;
        if settings.enabled {
            return Err(RuntimeError::InvalidInput(
                "relay is enabled but not connected".to_string(),
            ));
        }
        Ok(ActiveGithubAppBackend::Direct(Arc::clone(&self.direct)))
    }
}

enum ActiveGithubAppBackend {
    Direct(Arc<dyn GithubAppBackend>),
    Relay(Arc<RelayClient>),
}

#[async_trait::async_trait]
impl GithubAppBackend for DynamicGithubAppBackend {
    async fn github_app_settings(&self) -> mai_runtime::Result<GithubAppSettingsResponse> {
        match self.backend().await? {
            ActiveGithubAppBackend::Direct(backend) => backend.github_app_settings().await,
            ActiveGithubAppBackend::Relay(relay) => relay.github_app_settings().await,
        }
    }

    async fn save_github_app_settings(
        &self,
        request: GithubAppSettingsRequest,
    ) -> mai_runtime::Result<GithubAppSettingsResponse> {
        match self.backend().await? {
            ActiveGithubAppBackend::Direct(backend) => {
                backend.save_github_app_settings(request).await
            }
            ActiveGithubAppBackend::Relay(relay) => relay.save_github_app_settings(request).await,
        }
    }

    async fn start_github_app_manifest(
        &self,
        request: GithubAppManifestStartRequest,
    ) -> mai_runtime::Result<GithubAppManifestStartResponse> {
        match self.backend().await? {
            ActiveGithubAppBackend::Direct(backend) => {
                backend.start_github_app_manifest(request).await
            }
            ActiveGithubAppBackend::Relay(relay) => relay.start_github_app_manifest(request).await,
        }
    }

    async fn complete_github_app_manifest(
        &self,
        code: &str,
        state: &str,
    ) -> mai_runtime::Result<GithubAppSettingsResponse> {
        match self.backend().await? {
            ActiveGithubAppBackend::Direct(backend) => {
                backend.complete_github_app_manifest(code, state).await
            }
            ActiveGithubAppBackend::Relay(_) => Err(RuntimeError::InvalidInput(
                "GitHub App manifest callback is handled by mai-relay".to_string(),
            )),
        }
    }

    async fn list_github_installations(&self) -> mai_runtime::Result<GithubInstallationsResponse> {
        match self.backend().await? {
            ActiveGithubAppBackend::Direct(backend) => backend.list_github_installations().await,
            ActiveGithubAppBackend::Relay(relay) => relay.list_github_installations().await,
        }
    }

    async fn refresh_github_installations(
        &self,
    ) -> mai_runtime::Result<GithubInstallationsResponse> {
        match self.backend().await? {
            ActiveGithubAppBackend::Direct(backend) => backend.refresh_github_installations().await,
            ActiveGithubAppBackend::Relay(relay) => relay.list_github_installations().await,
        }
    }

    async fn list_github_repositories(
        &self,
        installation_id: u64,
    ) -> mai_runtime::Result<GithubRepositoriesResponse> {
        match self.backend().await? {
            ActiveGithubAppBackend::Direct(backend) => {
                backend.list_github_repositories(installation_id).await
            }
            ActiveGithubAppBackend::Relay(relay) => {
                relay.list_github_repositories(installation_id).await
            }
        }
    }

    async fn get_github_repository(
        &self,
        installation_id: u64,
        repository_full_name: &str,
    ) -> mai_runtime::Result<GithubRepositorySummary> {
        match self.backend().await? {
            ActiveGithubAppBackend::Direct(backend) => {
                backend
                    .get_github_repository(installation_id, repository_full_name)
                    .await
            }
            ActiveGithubAppBackend::Relay(relay) => {
                relay
                    .get_github_repository(installation_id, repository_full_name)
                    .await
            }
        }
    }

    async fn github_installation_token(
        &self,
        installation_id: u64,
        repository_id: Option<u64>,
        include_packages: bool,
    ) -> mai_runtime::Result<RelayGithubInstallationTokenResponse> {
        match self.backend().await? {
            ActiveGithubAppBackend::Direct(backend) => {
                backend
                    .github_installation_token(installation_id, repository_id, include_packages)
                    .await
            }
            ActiveGithubAppBackend::Relay(relay) => {
                relay
                    .create_installation_token(installation_id, repository_id, include_packages)
                    .await
            }
        }
    }
}

pub(crate) async fn list_relay_repository_packages(
    relay: Arc<RelayClient>,
    installation_id: u64,
    owner: &str,
    repo: &str,
) -> mai_runtime::Result<mai_protocol::RepositoryPackagesResponse> {
    relay
        .list_github_repository_packages(GithubAppInstallationPackagesRequest {
            installation_id,
            owner: owner.to_string(),
            repo: repo.to_string(),
        })
        .await
}
