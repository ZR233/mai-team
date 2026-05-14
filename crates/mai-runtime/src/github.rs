use chrono::{DateTime, Utc};
use mai_protocol::{
    GitTokenKind, GithubAppManifestAccountType, GithubInstallationSummary, GithubRepositorySummary,
    RepositoryPackageSummary, RepositoryPackagesResponse, preview,
};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    DEFAULT_GITHUB_API_BASE_URL, DEFAULT_GITHUB_WEB_BASE_URL, GITHUB_API_VERSION, Result,
    RuntimeError,
};

#[derive(Debug, Clone)]
pub(crate) struct GithubManifestState {
    pub(crate) created_at: Instant,
    pub(crate) account_type: GithubAppManifestAccountType,
    pub(crate) org: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubAppManifestUrls {
    pub(crate) redirect_url: String,
    pub(crate) setup_url: String,
    pub(crate) webhook_url: String,
    pub(crate) action_url: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct GithubJwtClaims {
    pub(crate) iat: usize,
    pub(crate) exp: usize,
    pub(crate) iss: String,
}

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

pub(crate) fn github_package_belongs_to_repo(
    package: &GithubPackageApi,
    repository_full_name: &str,
) -> bool {
    package.repository.as_ref().is_some_and(|repository| {
        repository
            .full_name
            .eq_ignore_ascii_case(repository_full_name)
    })
}

pub(crate) fn repository_package_summary(
    owner: &str,
    package: GithubPackageApi,
    versions: Vec<GithubPackageVersionApi>,
) -> Option<RepositoryPackageSummary> {
    let tag = preferred_container_tag(&versions)?;
    let image = format!("ghcr.io/{}/{}:{}", owner, package.name, tag);
    Some(RepositoryPackageSummary {
        name: package.name,
        image,
        tag,
        html_url: package.html_url,
    })
}

pub(crate) fn dedupe_github_packages(packages: Vec<GithubPackageApi>) -> Vec<GithubPackageApi> {
    let mut seen = std::collections::HashMap::new();
    let mut deduped = Vec::new();
    for package in packages {
        let key = github_package_key(&package);
        if seen.insert(key, ()).is_none() {
            deduped.push(package);
        }
    }
    deduped
}

fn github_package_key(package: &GithubPackageApi) -> String {
    let repository = package
        .repository
        .as_ref()
        .map(|repository| repository.full_name.as_str())
        .unwrap_or("");
    format!(
        "{}:{}",
        repository.to_ascii_lowercase(),
        package.name.to_ascii_lowercase()
    )
}

pub(crate) fn github_packages_read_error(status: Option<reqwest::StatusCode>) -> bool {
    matches!(
        status,
        Some(
            reqwest::StatusCode::BAD_REQUEST
                | reqwest::StatusCode::FORBIDDEN
                | reqwest::StatusCode::NOT_FOUND
        )
    )
}

pub(crate) async fn repository_packages_with_token(
    http: &reqwest::Client,
    api_base_url: &str,
    token: &str,
    owner: &str,
    repo: &str,
) -> Result<RepositoryPackagesResponse> {
    let owner = owner.trim();
    let repo = repo.trim();
    let repository_ref = format!("{owner}/{repo}");
    if owner.is_empty() || repo.is_empty() {
        return Err(RuntimeError::InvalidInput(
            "repository owner and name are required".to_string(),
        ));
    }
    let packages = match github_container_packages_for_owner(http, api_base_url, token, owner).await
    {
        Ok(packages) => dedupe_github_packages(
            packages
                .into_iter()
                .filter(|package| github_package_belongs_to_repo(package, &repository_ref))
                .collect(),
        ),
        Err(err) if github_packages_read_error(err.status()) => Vec::new(),
        Err(err) => return Err(RuntimeError::Http(err)),
    };
    let warning = if packages.is_empty() {
        Some("No readable GitHub container packages found for this repository".to_string())
    } else {
        None
    };
    let mut summaries = Vec::new();
    for package in packages
        .into_iter()
        .filter(|package| github_package_belongs_to_repo(package, &repository_ref))
    {
        let versions = match github_container_package_versions(
            http,
            api_base_url,
            token,
            owner,
            &package.name,
        )
        .await
        {
            Ok(versions) => versions,
            Err(err) if github_packages_read_error(err.status()) => continue,
            Err(err) => return Err(RuntimeError::Http(err)),
        };
        if let Some(summary) = repository_package_summary(owner, package, versions) {
            summaries.push(summary);
        }
    }
    summaries.sort_by(|left, right| left.name.cmp(&right.name).then(left.tag.cmp(&right.tag)));
    Ok(RepositoryPackagesResponse {
        packages: summaries,
        warning,
    })
}

async fn github_container_packages_for_owner(
    http: &reqwest::Client,
    api_base_url: &str,
    token: &str,
    owner: &str,
) -> std::result::Result<Vec<GithubPackageApi>, reqwest::Error> {
    let org_url = github_api_url(
        api_base_url,
        &format!(
            "/orgs/{}/packages?package_type=container&per_page=100",
            github_path_segment(owner)
        ),
    );
    let org_response = http
        .get(org_url)
        .bearer_auth(token)
        .headers(github_headers())
        .send()
        .await?;
    if org_response.status() != reqwest::StatusCode::NOT_FOUND {
        return org_response.error_for_status()?.json().await;
    }
    let user_url = github_api_url(
        api_base_url,
        &format!(
            "/users/{}/packages?package_type=container&per_page=100",
            github_path_segment(owner)
        ),
    );
    http.get(user_url)
        .bearer_auth(token)
        .headers(github_headers())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
}

async fn github_container_package_versions(
    http: &reqwest::Client,
    api_base_url: &str,
    token: &str,
    owner: &str,
    package_name: &str,
) -> std::result::Result<Vec<GithubPackageVersionApi>, reqwest::Error> {
    let org_url = github_api_url(
        api_base_url,
        &format!(
            "/orgs/{}/packages/container/{}/versions?per_page=30",
            github_path_segment(owner),
            github_path_segment(package_name)
        ),
    );
    let org_response = http
        .get(org_url)
        .bearer_auth(token)
        .headers(github_headers())
        .send()
        .await?;
    if org_response.status() != reqwest::StatusCode::NOT_FOUND {
        return org_response.error_for_status()?.json().await;
    }
    let user_url = github_api_url(
        api_base_url,
        &format!(
            "/users/{}/packages/container/{}/versions?per_page=30",
            github_path_segment(owner),
            github_path_segment(package_name)
        ),
    );
    http.get(user_url)
        .bearer_auth(token)
        .headers(github_headers())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
}

fn preferred_container_tag(versions: &[GithubPackageVersionApi]) -> Option<String> {
    let mut first_tag = None;
    for version in versions {
        for tag in &version.metadata.container.tags {
            let tag = tag.trim();
            if tag.is_empty() {
                continue;
            }
            if tag == "latest" {
                return Some(tag.to_string());
            }
            if first_tag.is_none() {
                first_tag = Some(tag.to_string());
            }
        }
    }
    first_tag
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

pub(crate) fn sanitize_origin(origin: &str) -> Result<String> {
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

pub(crate) fn is_valid_github_manifest_code(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub(crate) fn is_valid_github_slug(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.len() <= 100
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub(crate) fn github_app_manifest_urls(
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

pub(crate) fn github_app_manifest(redirect_url: &str, setup_url: &str, webhook_url: &str) -> Value {
    json!({
        "name": format!("Mai Team {}", Uuid::new_v4().to_string().split('-').next().unwrap_or("project")),
        "url": "https://github.com",
        "redirect_url": redirect_url,
        "callback_urls": [redirect_url],
        "setup_url": setup_url,
        "public": false,
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

pub(crate) fn github_manifest_owner_login(
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

pub(crate) fn github_manifest_owner_type(
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

pub(crate) fn github_installation_token_cache_key(
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

pub(crate) fn github_access_token_request(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_package_summary_prefers_latest_tag() {
        let package = GithubPackageApi {
            name: "mai-team-agent".to_string(),
            html_url: "https://github.com/orgs/example/packages/container/mai-team-agent"
                .to_string(),
            repository: Some(GithubPackageRepositoryApi {
                full_name: "example/mai-team".to_string(),
            }),
        };
        let versions = vec![
            GithubPackageVersionApi {
                metadata: GithubPackageVersionMetadataApi {
                    container: GithubPackageContainerMetadataApi {
                        tags: vec!["v1.2.0".to_string()],
                    },
                },
            },
            GithubPackageVersionApi {
                metadata: GithubPackageVersionMetadataApi {
                    container: GithubPackageContainerMetadataApi {
                        tags: vec!["latest".to_string(), "sha-123".to_string()],
                    },
                },
            },
        ];

        let summary = repository_package_summary("example", package, versions).expect("summary");

        assert_eq!(summary.tag, "latest");
        assert_eq!(summary.image, "ghcr.io/example/mai-team-agent:latest");
    }

    #[test]
    fn repository_package_summary_uses_first_available_tag() {
        let package = GithubPackageApi {
            name: "mai-team-sidecar".to_string(),
            html_url: "https://github.com/orgs/example/packages/container/mai-team-sidecar"
                .to_string(),
            repository: Some(GithubPackageRepositoryApi {
                full_name: "example/mai-team".to_string(),
            }),
        };
        let versions = vec![GithubPackageVersionApi {
            metadata: GithubPackageVersionMetadataApi {
                container: GithubPackageContainerMetadataApi {
                    tags: vec!["v1.2.0".to_string(), "sha-456".to_string()],
                },
            },
        }];

        let summary = repository_package_summary("example", package, versions).expect("summary");

        assert_eq!(summary.tag, "v1.2.0");
        assert_eq!(summary.image, "ghcr.io/example/mai-team-sidecar:v1.2.0");
    }

    #[test]
    fn github_package_match_requires_exact_repository() {
        let package = GithubPackageApi {
            name: "mai-team-agent".to_string(),
            html_url: "https://github.com/orgs/example/packages/container/mai-team-agent"
                .to_string(),
            repository: Some(GithubPackageRepositoryApi {
                full_name: "example/mai-team".to_string(),
            }),
        };
        let missing_repo_package = GithubPackageApi {
            name: "orphan-image".to_string(),
            html_url: "https://github.com/orgs/example/packages/container/orphan-image".to_string(),
            repository: None,
        };

        assert!(github_package_belongs_to_repo(&package, "example/mai-team"));
        assert!(!github_package_belongs_to_repo(&package, "example/other"));
        assert!(!github_package_belongs_to_repo(
            &missing_repo_package,
            "example/mai-team"
        ));
    }

    #[test]
    fn github_packages_bad_request_is_read_warning() {
        assert!(github_packages_read_error(Some(
            reqwest::StatusCode::BAD_REQUEST
        )));
        assert!(github_packages_read_error(Some(
            reqwest::StatusCode::FORBIDDEN
        )));
        assert!(github_packages_read_error(Some(
            reqwest::StatusCode::NOT_FOUND
        )));
        assert!(!github_packages_read_error(Some(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        )));
    }

    #[test]
    fn dedupe_github_packages_merges_repo_and_owner_sources() {
        let packages = dedupe_github_packages(vec![
            GithubPackageApi {
                name: "sidecar".to_string(),
                html_url: "https://github.com/users/example/packages/container/sidecar".to_string(),
                repository: Some(GithubPackageRepositoryApi {
                    full_name: "example/repo".to_string(),
                }),
            },
            GithubPackageApi {
                name: "SIDECAR".to_string(),
                html_url: "https://github.com/users/example/packages/container/SIDECAR".to_string(),
                repository: Some(GithubPackageRepositoryApi {
                    full_name: "Example/Repo".to_string(),
                }),
            },
        ]);

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "sidecar");
    }
}
