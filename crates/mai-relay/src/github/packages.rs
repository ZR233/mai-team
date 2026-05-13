use crate::error::{RelayErrorKind, RelayResult};
use crate::state::AppState;
use mai_protocol::{
    RelayGithubInstallationTokenRequest, RelayGithubRepositoryPackagesRequest,
    RepositoryPackageSummary, RepositoryPackagesResponse,
};
use std::collections::HashMap;

use super::types::{GithubPackageApi, GithubPackageVersionApi};

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
            Ok(packages) => dedupe_github_packages(
                packages
                    .into_iter()
                    .filter(|package| github_package_belongs_to_repo(package, &repository_ref))
                    .collect(),
            ),
            Err(err) if github_packages_read_error(err.status()) => Vec::new(),
            Err(err) => return Err(RelayErrorKind::Http(err)),
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
        let versions = match super::api::github_container_package_versions(
            state,
            &token.token,
            owner,
            &package.name,
        )
        .await
        {
            Ok(versions) => versions,
            Err(err) if github_packages_read_error(err.status()) => continue,
            Err(err) => return Err(RelayErrorKind::Http(err)),
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
            reqwest::StatusCode::BAD_REQUEST
                | reqwest::StatusCode::FORBIDDEN
                | reqwest::StatusCode::NOT_FOUND
        )
    )
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
