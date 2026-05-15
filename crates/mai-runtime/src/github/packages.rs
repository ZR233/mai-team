use mai_protocol::{RepositoryPackageSummary, RepositoryPackagesResponse};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};

use super::{github_api_url, github_headers, github_path_segment};
use crate::{Result, RuntimeError};

const GITHUB_PACKAGES_EMPTY_WARNING: &str =
    "No readable GitHub container packages found for this repository";
const GITHUB_PACKAGES_PERMISSION_WARNING: &str = "GitHub package listing requires `packages: read` on the GitHub App or `read:packages` on the Git token.";

#[derive(Debug, Clone, Deserialize)]
struct GithubPackageApi {
    name: String,
    html_url: String,
    #[serde(default)]
    repository: Option<GithubPackageRepositoryApi>,
}

#[derive(Debug, Clone, Deserialize)]
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
        Ok(packages) => {
            let explicit_matches = github_packages_with_repository(&packages, &repository_ref);
            if !explicit_matches.is_empty() {
                explicit_matches
            } else {
                let repository_package_names =
                    github_repository_package_names(http, api_base_url, token, owner, repo)
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
        Err(err) => return Err(RuntimeError::Http(err)),
    };
    let mut read_warning = None;
    let mut summaries = Vec::new();
    for package in packages {
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
            Err(err) if github_packages_read_error(err.status()) => {
                if let Some(warning) = github_packages_warning_for_read_error(err.status()) {
                    read_warning.get_or_insert(warning);
                }
                continue;
            }
            Err(err) => return Err(RuntimeError::Http(err)),
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

fn github_package_belongs_to_repo(package: &GithubPackageApi, repository_full_name: &str) -> bool {
    package.repository.as_ref().is_some_and(|repository| {
        repository
            .full_name
            .eq_ignore_ascii_case(repository_full_name)
    })
}

fn github_packages_for_repository(
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

fn github_packages_with_repository(
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

fn github_package_name_matches_repo(package_name: &str, repo: &str) -> bool {
    let package_name = package_name.to_ascii_lowercase();
    let repo = repo.to_ascii_lowercase();
    package_name == repo
        || package_name
            .strip_prefix(&repo)
            .is_some_and(|suffix| suffix.starts_with('-') || suffix.starts_with('_'))
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
            reqwest::StatusCode::UNAUTHORIZED
                | reqwest::StatusCode::BAD_REQUEST
                | reqwest::StatusCode::FORBIDDEN
                | reqwest::StatusCode::NOT_FOUND
        )
    )
}

fn github_packages_warning_for_read_error(status: Option<reqwest::StatusCode>) -> Option<String> {
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

async fn github_repository_package_names(
    http: &reqwest::Client,
    api_base_url: &str,
    token: &str,
    owner: &str,
    repo: &str,
) -> std::result::Result<Vec<String>, reqwest::Error> {
    let url = github_api_url(api_base_url, "/graphql");
    let response = http
        .post(url)
        .bearer_auth(token)
        .headers(github_headers())
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
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Mutex;

    async fn start_github_packages_mock(
        responses: Vec<(u16, serde_json::Value)>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock server addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let server_requests = Arc::clone(&requests);
        let server_responses = Arc::clone(&responses);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&server_requests);
                let responses = Arc::clone(&server_responses);
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    loop {
                        let read = stream.read(&mut chunk).await.expect("read request");
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let headers = String::from_utf8_lossy(&buffer);
                    requests
                        .lock()
                        .await
                        .push(headers.lines().next().unwrap_or_default().to_string());
                    let (status, value) = responses
                        .lock()
                        .await
                        .pop_front()
                        .unwrap_or_else(|| (200, json!([])));
                    let body = serde_json::to_string(&value).expect("response json");
                    let reason = if status == 200 { "OK" } else { "ERROR" };
                    let reply = format!(
                        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream
                        .write_all(reply.as_bytes())
                        .await
                        .expect("write response");
                });
            }
        });
        (format!("http://{addr}"), requests)
    }

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

    #[tokio::test]
    async fn repository_packages_with_token_returns_permission_warning_for_forbidden_listing() {
        let (base_url, requests) = start_github_packages_mock(vec![(
            403,
            json!({
                "message": "You need at least read:packages scope to list packages."
            }),
        )])
        .await;

        let response = repository_packages_with_token(
            &reqwest::Client::new(),
            &base_url,
            "token",
            "octo",
            "repo",
        )
        .await
        .expect("packages response");

        assert_eq!(response.packages, Vec::<RepositoryPackageSummary>::new());
        assert_eq!(
            response.warning,
            Some(
                "GitHub package listing requires `packages: read` on the GitHub App or `read:packages` on the Git token."
                    .to_string()
            )
        );
        assert_eq!(
            requests.lock().await.as_slice(),
            &["GET /orgs/octo/packages?package_type=container&per_page=100 HTTP/1.1"]
        );
    }

    #[tokio::test]
    async fn repository_packages_with_token_uses_graphql_repo_packages_when_rest_lacks_repository()
    {
        let (base_url, requests) = start_github_packages_mock(vec![
            (
                200,
                json!([
                    {
                        "name": "tgoskits-container",
                        "html_url": "https://github.com/orgs/rcore-os/packages/container/package/tgoskits-container"
                    },
                    {
                        "name": "unrelated",
                        "html_url": "https://github.com/orgs/rcore-os/packages/container/package/unrelated"
                    }
                ]),
            ),
            (
                200,
                json!({
                    "data": {
                        "repository": {
                            "packages": {
                                "nodes": [
                                    {
                                        "name": "tgoskits-container"
                                    }
                                ]
                            }
                        }
                    }
                }),
            ),
            (
                200,
                json!([
                    {
                        "metadata": {
                            "container": {
                                "tags": ["container-test-20260313-01", "latest"]
                            }
                        }
                    }
                ]),
            ),
        ])
        .await;

        let response = repository_packages_with_token(
            &reqwest::Client::new(),
            &base_url,
            "token",
            "rcore-os",
            "tgoskits",
        )
        .await
        .expect("packages response");

        assert_eq!(
            response.packages,
            vec![RepositoryPackageSummary {
                name: "tgoskits-container".to_string(),
                image: "ghcr.io/rcore-os/tgoskits-container:latest".to_string(),
                tag: "latest".to_string(),
                html_url:
                    "https://github.com/orgs/rcore-os/packages/container/package/tgoskits-container"
                        .to_string(),
            }]
        );
        assert_eq!(response.warning, None);
        assert_eq!(
            requests.lock().await.as_slice(),
            &[
                "GET /orgs/rcore%2Dos/packages?package_type=container&per_page=100 HTTP/1.1",
                "POST /graphql HTTP/1.1",
                "GET /orgs/rcore%2Dos/packages/container/tgoskits%2Dcontainer/versions?per_page=30 HTTP/1.1",
            ]
        );
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
