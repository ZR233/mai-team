use mai_protocol::{RepositoryPackageSummary, RepositoryPackagesResponse};
use serde::Deserialize;

use super::{github_api_url, github_headers, github_path_segment};
use crate::{Result, RuntimeError};

#[derive(Debug, Deserialize)]
struct GithubPackageApi {
    name: String,
    html_url: String,
    #[serde(default)]
    repository: Option<GithubPackageRepositoryApi>,
}

#[derive(Debug, Deserialize)]
struct GithubPackageRepositoryApi {
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct GithubPackageVersionApi {
    #[serde(default)]
    metadata: GithubPackageVersionMetadataApi,
}

#[derive(Debug, Deserialize, Default)]
struct GithubPackageVersionMetadataApi {
    #[serde(default)]
    container: GithubPackageContainerMetadataApi,
}

#[derive(Debug, Deserialize, Default)]
struct GithubPackageContainerMetadataApi {
    #[serde(default)]
    tags: Vec<String>,
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

fn github_package_belongs_to_repo(package: &GithubPackageApi, repository_full_name: &str) -> bool {
    package.repository.as_ref().is_some_and(|repository| {
        repository
            .full_name
            .eq_ignore_ascii_case(repository_full_name)
    })
}

fn repository_package_summary(
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

fn dedupe_github_packages(packages: Vec<GithubPackageApi>) -> Vec<GithubPackageApi> {
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

fn github_packages_read_error(status: Option<reqwest::StatusCode>) -> bool {
    matches!(
        status,
        Some(
            reqwest::StatusCode::BAD_REQUEST
                | reqwest::StatusCode::FORBIDDEN
                | reqwest::StatusCode::NOT_FOUND
        )
    )
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
