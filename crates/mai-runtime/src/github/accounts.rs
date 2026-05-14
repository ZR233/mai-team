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
        let token = self.token(account_id).await?;
        repository_packages_with_token(&self.http, &self.api_base_url, &token, owner, repo).await
    }

    pub(crate) async fn verified_repository(
        &self,
        account_id: &str,
        repository_full_name: &str,
    ) -> Result<VerifiedGithubRepository> {
        let token = self.token(account_id).await?;
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
        let account = self.summary(account_id).await?;
        if account.provider == GitProvider::GithubAppRelay {
            let installation_id = account.installation_id.ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "relay git account installation_id is missing".to_string(),
                )
            })?;
            return Ok(self
                .github_backend
                .github_installation_token(installation_id, None, false)
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

fn redact_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        return value.to_string();
    }
    value.replace(secret, "<redacted>")
}
