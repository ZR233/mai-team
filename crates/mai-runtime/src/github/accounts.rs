use std::sync::Arc;

use mai_protocol::{
    GitAccountRequest, GitAccountResponse, GitAccountStatus, GitAccountSummary,
    GitAccountsResponse, GitProvider, GitTokenKind, GithubRepositoriesResponse,
    GithubRepositorySummary, RepositoryPackagesResponse,
};
use mai_store::ConfigStore;

use super::{
    GithubRepositoryApi, GithubUserApi, decode_github_response, git_token_kind, github_api_url,
    github_headers, github_repository_summary, github_scopes, repository_packages_with_token,
};
use crate::{GithubAppBackend, Result, RuntimeError};

#[derive(Debug, Clone)]
pub(crate) struct VerifiedGithubRepository {
    pub(crate) id: u64,
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) full_name: String,
    pub(crate) default_branch: String,
}

#[derive(Debug, Clone, Copy)]
enum GitAccountTokenUse {
    Default,
    PackagesRead,
}

impl GitAccountTokenUse {
    fn include_packages(self) -> bool {
        matches!(self, Self::PackagesRead)
    }
}

#[derive(Clone)]
pub(crate) struct GitAccountService {
    store: Arc<ConfigStore>,
    http: reqwest::Client,
    api_base_url: String,
    github_backend: Arc<dyn GithubAppBackend>,
}

impl GitAccountService {
    pub(crate) fn new(
        store: Arc<ConfigStore>,
        http: reqwest::Client,
        api_base_url: String,
        github_backend: Arc<dyn GithubAppBackend>,
    ) -> Self {
        Self {
            store,
            http,
            api_base_url,
            github_backend,
        }
    }

    pub(crate) async fn list(&self) -> Result<GitAccountsResponse> {
        Ok(self.store.list_git_accounts().await?)
    }

    pub(crate) async fn save(
        self: &Arc<Self>,
        request: GitAccountRequest,
    ) -> Result<GitAccountResponse> {
        let account = self.store.upsert_git_account(request).await?;
        let service = Arc::clone(self);
        let account_id = account.id.clone();
        tokio::spawn(async move {
            if let Err(err) = service.verify(&account_id).await {
                tracing::warn!(account_id = %account_id, "failed to verify git account in background: {err}");
            }
        });
        Ok(GitAccountResponse { account })
    }

    pub(crate) async fn verify(&self, account_id: &str) -> Result<GitAccountSummary> {
        let token = self.token(account_id).await?;
        self.store.mark_git_account_verifying(account_id).await?;
        let response = match self
            .http
            .get(github_api_url(&self.api_base_url, "/user"))
            .bearer_auth(&token)
            .headers(github_headers())
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                return Ok(self
                    .store
                    .update_git_account_verification(
                        account_id,
                        None,
                        GitTokenKind::Unknown,
                        Vec::new(),
                        GitAccountStatus::Failed,
                        Some(redact_secret(&err.to_string(), &token)),
                    )
                    .await?);
            }
        };
        let scopes = github_scopes(response.headers());
        let token_kind = git_token_kind(&token, &scopes);
        match decode_github_response::<GithubUserApi>(response, "verify token").await {
            Ok(user) => Ok(self
                .store
                .update_git_account_verification(
                    account_id,
                    Some(user.login),
                    token_kind,
                    scopes,
                    GitAccountStatus::Verified,
                    None,
                )
                .await?),
            Err(err) => Ok(self
                .store
                .update_git_account_verification(
                    account_id,
                    None,
                    token_kind,
                    scopes,
                    GitAccountStatus::Failed,
                    Some(redact_secret(&err.to_string(), &token)),
                )
                .await?),
        }
    }

    pub(crate) async fn delete(&self, account_id: &str) -> Result<GitAccountsResponse> {
        Ok(self.store.delete_git_account(account_id).await?)
    }

    pub(crate) async fn set_default(&self, account_id: &str) -> Result<GitAccountsResponse> {
        Ok(self.store.set_default_git_account(account_id).await?)
    }

    pub(crate) async fn list_repositories(
        &self,
        account_id: &str,
    ) -> Result<GithubRepositoriesResponse> {
        let token = self.token(account_id).await?;
        let url = github_api_url(
            &self.api_base_url,
            "/user/repos?per_page=100&affiliation=owner,collaborator,organization_member&sort=updated",
        );
        let response = self
            .http
            .get(url)
            .bearer_auth(&token)
            .headers(github_headers())
            .send()
            .await?;
        let repositories: Vec<GithubRepositoryApi> =
            decode_github_response(response, "list repositories").await?;
        Ok(GithubRepositoriesResponse {
            repositories: repositories
                .into_iter()
                .map(github_repository_summary)
                .collect::<Vec<GithubRepositorySummary>>(),
        })
    }

    pub(crate) async fn list_repository_packages(
        &self,
        account_id: &str,
        owner: &str,
        repo: &str,
    ) -> Result<RepositoryPackagesResponse> {
        let token = self
            .token_for(account_id, GitAccountTokenUse::PackagesRead)
            .await?;
        repository_packages_with_token(&self.http, &self.api_base_url, &token, owner, repo).await
    }

    pub(crate) async fn verified_repository(
        &self,
        account_id: &str,
        repository_full_name: &str,
    ) -> Result<VerifiedGithubRepository> {
        let account = self.summary(account_id).await?;
        let repository_full_name = repository_full_name.trim();
        if !repository_full_name.contains('/') || repository_full_name.contains(char::is_whitespace)
        {
            return Err(RuntimeError::InvalidInput(
                "repository_full_name must look like owner/repo".to_string(),
            ));
        }
        if account.provider == GitProvider::GithubAppRelay {
            let installation_id = account.installation_id.ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "relay git account installation_id is missing".to_string(),
                )
            })?;
            let repository = self
                .github_backend
                .get_github_repository(installation_id, repository_full_name)
                .await?;
            return Ok(VerifiedGithubRepository {
                id: repository.id,
                owner: repository.owner,
                name: repository.name,
                full_name: repository.full_name,
                default_branch: repository
                    .default_branch
                    .unwrap_or_else(|| "main".to_string()),
            });
        }
        let token = self
            .token_for(account_id, GitAccountTokenUse::Default)
            .await?;
        let url = github_api_url(
            &self.api_base_url,
            &format!("/repos/{repository_full_name}"),
        );
        let response = self
            .http
            .get(url)
            .bearer_auth(&token)
            .headers(github_headers())
            .send()
            .await?;
        let repository: GithubRepositoryApi =
            decode_github_response(response, "get repository").await?;
        Ok(VerifiedGithubRepository {
            id: repository.id,
            owner: repository.owner.login,
            name: repository.name,
            full_name: repository.full_name,
            default_branch: repository
                .default_branch
                .unwrap_or_else(|| "main".to_string()),
        })
    }

    pub(crate) async fn summary(&self, account_id: &str) -> Result<GitAccountSummary> {
        self.store
            .git_account(account_id)
            .await?
            .ok_or_else(|| RuntimeError::InvalidInput("git account not found".to_string()))
    }

    pub(crate) async fn token(&self, account_id: &str) -> Result<String> {
        self.token_for(account_id, GitAccountTokenUse::Default)
            .await
    }

    async fn token_for(&self, account_id: &str, token_use: GitAccountTokenUse) -> Result<String> {
        let account = self.summary(account_id).await?;
        if account.provider == GitProvider::GithubAppRelay {
            let installation_id = account.installation_id.ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "relay git account installation_id is missing".to_string(),
                )
            })?;
            return Ok(self
                .github_backend
                .github_installation_token(installation_id, None, token_use.include_packages())
                .await?
                .token);
        }
        self.store
            .git_account_token(account_id)
            .await?
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| RuntimeError::InvalidInput("git account token not found".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::GithubAppBackend;
    use async_trait::async_trait;
    use chrono::{TimeDelta, Utc};
    use mai_protocol::{
        GithubAppManifestStartRequest, GithubAppManifestStartResponse, GithubAppSettingsRequest,
        GithubAppSettingsResponse, GithubInstallationSummary, GithubInstallationsResponse,
        GithubRepositoriesResponse, RelayGithubInstallationTokenResponse, RepositoryPackageSummary,
    };
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::collections::VecDeque;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct MockGithubAppBackend {
        token_requests: Mutex<Vec<(u64, Option<u64>, bool)>>,
    }

    #[async_trait]
    impl GithubAppBackend for MockGithubAppBackend {
        async fn github_app_settings(&self) -> Result<GithubAppSettingsResponse> {
            unimplemented!("not needed by this test")
        }

        async fn save_github_app_settings(
            &self,
            _request: GithubAppSettingsRequest,
        ) -> Result<GithubAppSettingsResponse> {
            unimplemented!("not needed by this test")
        }

        async fn start_github_app_manifest(
            &self,
            _request: GithubAppManifestStartRequest,
        ) -> Result<GithubAppManifestStartResponse> {
            unimplemented!("not needed by this test")
        }

        async fn complete_github_app_manifest(
            &self,
            _code: &str,
            _state: &str,
        ) -> Result<GithubAppSettingsResponse> {
            unimplemented!("not needed by this test")
        }

        async fn list_github_installations(&self) -> Result<GithubInstallationsResponse> {
            Ok(GithubInstallationsResponse {
                installations: Vec::<GithubInstallationSummary>::new(),
            })
        }

        async fn refresh_github_installations(&self) -> Result<GithubInstallationsResponse> {
            self.list_github_installations().await
        }

        async fn list_github_repositories(
            &self,
            _installation_id: u64,
        ) -> Result<GithubRepositoriesResponse> {
            unimplemented!("not needed by this test")
        }

        async fn get_github_repository(
            &self,
            _installation_id: u64,
            _repository_full_name: &str,
        ) -> Result<GithubRepositorySummary> {
            unimplemented!("not needed by this test")
        }

        async fn github_installation_token(
            &self,
            installation_id: u64,
            repository_id: Option<u64>,
            include_packages: bool,
        ) -> Result<RelayGithubInstallationTokenResponse> {
            self.token_requests.lock().await.push((
                installation_id,
                repository_id,
                include_packages,
            ));
            Ok(RelayGithubInstallationTokenResponse {
                token: if include_packages {
                    "packages-token".to_string()
                } else {
                    "default-token".to_string()
                },
                expires_at: Utc::now() + TimeDelta::hours(1),
            })
        }
    }

    async fn test_store(dir: &tempfile::TempDir) -> Arc<ConfigStore> {
        Arc::new(
            ConfigStore::open_with_config_and_artifact_index_path(
                dir.path().join("runtime.sqlite3"),
                dir.path().join("config.toml"),
                dir.path().join("data/artifacts/index"),
            )
            .await
            .expect("open store"),
        )
    }

    async fn start_package_api_mock() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock server addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(VecDeque::from([
            json!([{
                "name": "repo-agent",
                "html_url": "https://github.com/orgs/octo/packages/container/repo-agent",
                "repository": {
                    "full_name": "octo/repo"
                }
            }])
            .to_string(),
            json!([{
                "metadata": {
                    "container": {
                        "tags": ["v1.0.0", "latest"]
                    }
                }
            }])
            .to_string(),
        ])));
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
                    let request_line = headers.lines().next().unwrap_or_default().to_string();
                    requests.lock().await.push(request_line);
                    let body = responses
                        .lock()
                        .await
                        .pop_front()
                        .unwrap_or_else(|| "[]".to_string());
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
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

    #[tokio::test]
    async fn relay_git_account_repository_packages_uses_packages_installation_token() {
        let (base_url, _requests) = start_package_api_mock().await;
        let dir = tempdir().expect("tempdir");
        let store = test_store(&dir).await;
        store
            .upsert_git_account(GitAccountRequest {
                id: Some("relay-account".to_string()),
                provider: GitProvider::GithubAppRelay,
                label: "Relay".to_string(),
                installation_id: Some(42),
                installation_account: Some("octo".to_string()),
                ..Default::default()
            })
            .await
            .expect("save account");
        let github_backend = Arc::new(MockGithubAppBackend::default());
        let service = GitAccountService::new(
            Arc::clone(&store),
            reqwest::Client::new(),
            base_url,
            github_backend.clone(),
        );

        let response = service
            .list_repository_packages("relay-account", "octo", "repo")
            .await
            .expect("list packages");

        assert_eq!(
            github_backend.token_requests.lock().await.as_slice(),
            &[(42, None, true)]
        );
        assert_eq!(
            response.packages,
            vec![RepositoryPackageSummary {
                name: "repo-agent".to_string(),
                image: "ghcr.io/octo/repo-agent:latest".to_string(),
                tag: "latest".to_string(),
                html_url: "https://github.com/orgs/octo/packages/container/repo-agent".to_string(),
            }]
        );
        assert_eq!(response.warning, None);
    }
}

fn redact_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        return value.to_string();
    }
    value.replace(secret, "<redacted>")
}
