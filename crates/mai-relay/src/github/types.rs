use chrono::{DateTime, Utc};
use mai_protocol::GithubAppManifestAccountType;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub(crate) struct GithubJwtClaims {
    pub(crate) iat: usize,
    pub(crate) exp: usize,
    pub(crate) iss: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubManifestCallbackQuery {
    pub(crate) code: Option<String>,
    pub(crate) state: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubInstallationCallbackQuery {
    pub(crate) setup_action: Option<String>,
    pub(crate) installation_id: Option<u64>,
    pub(crate) state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GithubAppConfig {
    pub(crate) app_id: String,
    pub(crate) private_key: String,
    #[serde(default)]
    pub(crate) webhook_secret: String,
    pub(crate) app_slug: Option<String>,
    pub(crate) app_html_url: Option<String>,
    pub(crate) owner_login: Option<String>,
    pub(crate) owner_type: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ManifestState {
    pub(crate) state: String,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) account_type: GithubAppManifestAccountType,
    pub(crate) org: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct InstallationState {
    pub(crate) state: String,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) origin: String,
    pub(crate) return_hash: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubAccountApi {
    pub(crate) login: String,
    #[serde(rename = "type")]
    pub(crate) account_type: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubInstallationApi {
    pub(crate) id: u64,
    pub(crate) account: GithubAccountApi,
    pub(crate) repository_selection: Option<String>,
    #[serde(default)]
    pub(crate) events: Vec<String>,
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
pub(crate) struct GithubPackageApi {
    pub(crate) name: String,
    pub(crate) html_url: String,
    #[serde(default)]
    pub(crate) repository: Option<GithubPackageRepositoryApi>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubPackageRepositoryApi {
    pub(crate) full_name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubPackageVersionApi {
    #[serde(default)]
    pub(crate) metadata: GithubPackageVersionMetadataApi,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct GithubPackageVersionMetadataApi {
    #[serde(default)]
    pub(crate) container: GithubPackageContainerMetadataApi,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct GithubPackageContainerMetadataApi {
    #[serde(default)]
    pub(crate) tags: Vec<String>,
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
    #[serde(default)]
    pub(crate) webhook_secret: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubAppApi {
    pub(crate) slug: String,
    pub(crate) html_url: String,
    #[serde(default)]
    pub(crate) owner: Option<GithubAccountApi>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubErrorResponse {
    pub(crate) message: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GithubAppHookConfigRequest {
    pub(crate) url: String,
    pub(crate) content_type: &'static str,
    pub(crate) insecure_ssl: &'static str,
    pub(crate) secret: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubAppHookConfigResponse {
    pub(crate) url: Option<String>,
}

#[derive(Debug)]
pub(crate) enum GithubHookReset {
    Updated,
    Missing,
}
