use crate::error::{RelayErrorKind, RelayResult};
use crate::state::AppState;
use mai_protocol::{
    RelayGithubInstallationTokenRequest, RelayGithubRepositoryPackagesRequest,
    RepositoryPackageSummary, RepositoryPackagesResponse,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};

use super::types::{GithubPackageApi, GithubPackageVersionApi};

const GITHUB_PACKAGES_EMPTY_WARNING: &str =
    "No readable GitHub container packages found for this repository";
const GITHUB_PACKAGES_PERMISSION_WARNING: &str = "GitHub package listing requires `packages: read` on the GitHub App or `read:packages` on the Git token.";

#[derive(Debug, Deserialize)]
struct GithubGraphqlResponse<T> {
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct GithubRepositoryPackagesGraphqlData {
    repository: Option<GithubRepositoryPackagesGraphqlRepository>,
}

#[derive(Debug, Deserialize)]
struct GithubRepositoryPackagesGraphqlRepository {
    packages: GithubRepositoryPackagesGraphqlConnection,
}

#[derive(Debug, Deserialize)]
struct GithubRepositoryPackagesGraphqlConnection {
    #[serde(default)]
    nodes: Vec<GithubRepositoryPackagesGraphqlPackage>,
}

#[derive(Debug, Deserialize)]
struct GithubRepositoryPackagesGraphqlPackage {
    name: String,
}

pub(crate) async fn list_repository_packages(
    state: &AppState,
    request: RelayGithubRepositoryPackagesRequest,
) -> RelayResult<RepositoryPackagesResponse> {
    if request.installation_id == 0 {
        return Err(RelayErrorKind::InvalidInput(
            "installation_id is required".to_string(),
        ));
    }
    let owner = request.owner.trim();
    let repo = request.repo.trim();
    if owner.is_empty() || repo.is_empty() {
        return Err(RelayErrorKind::InvalidInput(
            "repository owner and name are required".to_string(),
        ));
    }
    let token = super::api::create_installation_token(
        state,
        RelayGithubInstallationTokenRequest {
            installation_id: request.installation_id,
            repository_id: None,
            include_packages: true,
        },
    )
    .await?;
    let repository_ref = format!("{owner}/{repo}");
    let packages =
        match super::api::github_container_packages_for_owner(state, &token.token, owner).await {
            Ok(packages) => {
                let explicit_matches = github_packages_with_repository(&packages, &repository_ref);
                if !explicit_matches.is_empty() {
                    explicit_matches
                } else {
                    let repository_package_names =
                        github_repository_package_names(state, &token.token, owner, repo)
                            .await
                            .unwrap_or_default();
                    github_packages_for_repository(
                        packages,
                        &repository_ref,
                        repo,
                        &repository_package_names,
                    )
                }
            }
            Err(err) if github_packages_read_error(err.status()) => {
                return Ok(RepositoryPackagesResponse {
                    packages: Vec::new(),
                    warning: github_packages_warning_for_read_error(err.status()),
                });
            }
            Err(err) => return Err(RelayErrorKind::Http(err)),
        };
    let mut read_warning = None;
    let mut summaries = Vec::new();
    for package in packages {
        let versions = match super::api::github_container_package_versions(
            state,
            &token.token,
            owner,
            &package.name,
        )
        .await
        {
            Ok(versions) => versions,
            Err(err) if github_packages_read_error(err.status()) => {
                if let Some(warning) = github_packages_warning_for_read_error(err.status()) {
                    read_warning.get_or_insert(warning);
                }
                continue;
            }
            Err(err) => return Err(RelayErrorKind::Http(err)),
        };
        if let Some(summary) = repository_package_summary(owner, package, versions) {
            summaries.push(summary);
        }
    }
    summaries.sort_by(|left, right| left.name.cmp(&right.name).then(left.tag.cmp(&right.tag)));
    let warning = if summaries.is_empty() {
        read_warning.or_else(|| Some(GITHUB_PACKAGES_EMPTY_WARNING.to_string()))
    } else {
        None
    };
    Ok(RepositoryPackagesResponse {
        packages: summaries,
        warning,
    })
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

pub(crate) fn github_packages_for_repository(
    packages: Vec<GithubPackageApi>,
    repository_full_name: &str,
    repo: &str,
    repository_package_names: &[String],
) -> Vec<GithubPackageApi> {
    let explicit_matches = github_packages_with_repository(&packages, repository_full_name);
    if !explicit_matches.is_empty() {
        return explicit_matches;
    }
    let names = repository_package_names
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    if !names.is_empty() {
        let matches = dedupe_github_packages(
            packages
                .iter()
                .filter(|package| names.contains(&package.name.to_ascii_lowercase()))
                .cloned()
                .collect(),
        );
        if !matches.is_empty() {
            return matches;
        }
    }
    dedupe_github_packages(
        packages
            .into_iter()
            .filter(|package| github_package_name_matches_repo(&package.name, repo))
            .collect(),
    )
}

pub(crate) fn github_packages_with_repository(
    packages: &[GithubPackageApi],
    repository_full_name: &str,
) -> Vec<GithubPackageApi> {
    dedupe_github_packages(
        packages
            .iter()
            .filter(|package| github_package_belongs_to_repo(package, repository_full_name))
            .cloned()
            .collect(),
    )
}

pub(crate) fn github_package_name_matches_repo(package_name: &str, repo: &str) -> bool {
    let package_name = package_name.to_ascii_lowercase();
    let repo = repo.to_ascii_lowercase();
    package_name == repo
        || package_name
            .strip_prefix(&repo)
            .is_some_and(|suffix| suffix.starts_with('-') || suffix.starts_with('_'))
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
    let mut seen = HashMap::new();
    let mut deduped = Vec::new();
    for package in packages {
        let key = github_package_key(&package);
        if seen.insert(key, ()).is_none() {
            deduped.push(package);
        }
    }
    deduped
}

pub(crate) fn github_package_key(package: &GithubPackageApi) -> String {
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
            reqwest::StatusCode::UNAUTHORIZED
                | reqwest::StatusCode::BAD_REQUEST
                | reqwest::StatusCode::FORBIDDEN
                | reqwest::StatusCode::NOT_FOUND
        )
    )
}
pub(crate) fn github_packages_warning_for_read_error(
    status: Option<reqwest::StatusCode>,
) -> Option<String> {
    match status {
        Some(reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN) => {
            Some(GITHUB_PACKAGES_PERMISSION_WARNING.to_string())
        }
        Some(reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::NOT_FOUND) => {
            Some(GITHUB_PACKAGES_EMPTY_WARNING.to_string())
        }
        Some(_) | None => None,
    }
}
pub(crate) async fn github_repository_package_names(
    state: &AppState,
    token: &str,
    owner: &str,
    repo: &str,
) -> std::result::Result<Vec<String>, reqwest::Error> {
    let url = super::api::github_api_url(&state.github_api_base_url, "/graphql");
    let response = state
        .http
        .post(url)
        .bearer_auth(token)
        .headers(super::api::github_headers())
        .json(&json!({
            "query": r#"
                query RepositoryPackages($owner: String!, $repo: String!) {
                    repository(owner: $owner, name: $repo) {
                        packages(first: 100) {
                            nodes {
                                name
                            }
                        }
                    }
                }
            "#,
            "variables": {
                "owner": owner,
                "repo": repo,
            }
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<GithubGraphqlResponse<GithubRepositoryPackagesGraphqlData>>()
        .await?;
    Ok(response
        .data
        .and_then(|data| data.repository)
        .map(|repository| {
            repository
                .packages
                .nodes
                .into_iter()
                .map(|package| package.name)
                .collect()
        })
        .unwrap_or_default())
}
pub(crate) fn preferred_container_tag(versions: &[GithubPackageVersionApi]) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::super::types::GithubPackageRepositoryApi;
    use super::*;

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
    fn github_packages_auth_errors_explain_required_permission() {
        let warning = github_packages_warning_for_read_error(Some(reqwest::StatusCode::FORBIDDEN))
            .expect("warning");

        assert_eq!(
            warning,
            "GitHub package listing requires `packages: read` on the GitHub App or `read:packages` on the Git token."
        );
        assert!(
            github_packages_warning_for_read_error(Some(reqwest::StatusCode::UNAUTHORIZED))
                .is_some()
        );
    }

    #[test]
    fn github_packages_not_found_uses_empty_package_warning() {
        assert_eq!(
            github_packages_warning_for_read_error(Some(reqwest::StatusCode::NOT_FOUND)),
            Some("No readable GitHub container packages found for this repository".to_string())
        );
    }

    #[test]
    fn github_packages_for_repository_uses_graphql_names_when_rest_lacks_repository() {
        let packages = github_packages_for_repository(
            vec![
                GithubPackageApi {
                    name: "tgoskits-container".to_string(),
                    html_url: "https://github.com/orgs/rcore-os/packages/container/package/tgoskits-container"
                        .to_string(),
                    repository: None,
                },
                GithubPackageApi {
                    name: "unrelated".to_string(),
                    html_url: "https://github.com/orgs/rcore-os/packages/container/package/unrelated"
                        .to_string(),
                    repository: None,
                },
            ],
            "rcore-os/tgoskits",
            "tgoskits",
            &["tgoskits-container".to_string()],
        );

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "tgoskits-container");
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
