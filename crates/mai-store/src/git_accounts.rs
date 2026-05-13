use crate::*;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct GitAccountsConfig {
    #[serde(default)]
    default_account_id: Option<String>,
    #[serde(default)]
    accounts: Vec<StoredGitAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredGitAccount {
    id: String,
    #[serde(default)]
    provider: GitProvider,
    label: String,
    #[serde(default)]
    login: Option<String>,
    #[serde(default)]
    token_kind: GitTokenKind,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    status: GitAccountStatus,
    #[serde(default)]
    is_default: bool,
    token_secret: String,
    #[serde(default)]
    last_verified_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    installation_id: Option<u64>,
    #[serde(default)]
    installation_account: Option<String>,
    #[serde(default)]
    relay_id: Option<String>,
}

impl ConfigStore {
    pub async fn list_git_accounts(&self) -> Result<GitAccountsResponse> {
        let _guard = self.git_accounts_lock.lock().await;
        let config = self.git_accounts_config().await?;
        Ok(git_accounts_response(&config))
    }

    pub async fn upsert_git_account(
        &self,
        request: GitAccountRequest,
    ) -> Result<GitAccountSummary> {
        let _guard = self.git_accounts_lock.lock().await;
        let token = request.token.unwrap_or_default().trim().to_string();
        let mut config = self.git_accounts_config().await?;
        let id = request
            .id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let existing = config.accounts.iter().position(|account| account.id == id);
        let current_token = existing
            .and_then(|index| config.accounts.get(index))
            .map(|account| account.token_secret.clone())
            .unwrap_or_default();
        let has_new_token = !token.is_empty();
        let token_secret = if token.is_empty() {
            current_token
        } else {
            token
        };
        if token_secret.trim().is_empty() && request.provider != GitProvider::GithubAppRelay {
            return Err(StoreError::InvalidConfig(
                "git account token is required".to_string(),
            ));
        }
        if request.provider == GitProvider::GithubAppRelay
            && request.installation_id.unwrap_or_default() == 0
        {
            return Err(StoreError::InvalidConfig(
                "GitHub App relay account requires installation_id".to_string(),
            ));
        }
        let fallback_label = request
            .login
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("GitHub");
        let label = request
            .label
            .trim()
            .to_string()
            .if_empty(fallback_label)
            .to_string();
        let mut account = existing
            .and_then(|index| config.accounts.get(index).cloned())
            .unwrap_or_else(|| StoredGitAccount {
                id: id.clone(),
                provider: request.provider.clone(),
                label: label.clone(),
                login: None,
                token_kind: GitTokenKind::Unknown,
                scopes: Vec::new(),
                status: GitAccountStatus::Unverified,
                is_default: false,
                token_secret: token_secret.clone(),
                last_verified_at: None,
                last_error: None,
                installation_id: None,
                installation_account: None,
                relay_id: None,
            });
        account.provider = request.provider;
        account.label = label;
        if let Some(login) = request.login {
            account.login = Some(login.trim().to_string()).filter(|value| !value.is_empty());
        }
        account.token_secret = token_secret;
        account.status = if account.provider == GitProvider::GithubAppRelay {
            GitAccountStatus::Verified
        } else {
            GitAccountStatus::Verifying
        };
        account.last_error = None;
        account.installation_id = request.installation_id;
        account.installation_account = request
            .installation_account
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        account.relay_id = request
            .relay_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if account.provider == GitProvider::GithubAppRelay {
            account.token_kind = GitTokenKind::Unknown;
            account.scopes = Vec::new();
            account.last_verified_at = Some(Utc::now());
        }
        if has_new_token {
            account.last_verified_at = None;
        }
        if request.is_default || config.accounts.is_empty() {
            config.default_account_id = Some(id.clone());
        }
        account.is_default = config.default_account_id.as_deref() == Some(id.as_str());
        if let Some(index) = existing {
            config.accounts[index] = account;
        } else {
            config.accounts.push(account);
        }
        normalize_git_account_defaults(&mut config);
        self.save_git_accounts_config(&config).await?;
        Ok(config
            .accounts
            .iter()
            .find(|account| account.id == id)
            .map(|account| account.summary(config.default_account_id.as_deref()))
            .expect("saved account"))
    }

    pub async fn upsert_github_app_relay_account(
        &self,
        installation_id: u64,
        installation_account: &str,
        relay_id: &str,
        is_default: bool,
    ) -> Result<GitAccountSummary> {
        let id = format!("github-app-installation-{installation_id}");
        self.upsert_git_account(GitAccountRequest {
            id: Some(id),
            provider: GitProvider::GithubAppRelay,
            label: format!("GitHub App: {installation_account}"),
            login: Some(installation_account.to_string()),
            token: None,
            is_default,
            installation_id: Some(installation_id),
            installation_account: Some(installation_account.to_string()),
            relay_id: Some(relay_id.to_string()),
        })
        .await
    }

    pub async fn update_git_account_verification(
        &self,
        account_id: &str,
        login: Option<String>,
        token_kind: GitTokenKind,
        scopes: Vec<String>,
        status: GitAccountStatus,
        last_error: Option<String>,
    ) -> Result<GitAccountSummary> {
        let _guard = self.git_accounts_lock.lock().await;
        let mut config = self.git_accounts_config().await?;
        let default_account_id = config.default_account_id.clone();
        let account = config
            .accounts
            .iter_mut()
            .find(|account| account.id == account_id)
            .ok_or_else(|| StoreError::InvalidConfig("git account not found".to_string()))?;
        account.login = login.or_else(|| account.login.clone());
        account.token_kind = token_kind;
        account.scopes = scopes;
        account.status = status;
        account.last_verified_at = Some(Utc::now());
        account.last_error = last_error;
        let summary = account.summary(default_account_id.as_deref());
        self.save_git_accounts_config(&config).await?;
        Ok(summary)
    }

    pub async fn mark_git_account_verifying(&self, account_id: &str) -> Result<GitAccountSummary> {
        let _guard = self.git_accounts_lock.lock().await;
        let mut config = self.git_accounts_config().await?;
        let default_account_id = config.default_account_id.clone();
        let account = config
            .accounts
            .iter_mut()
            .find(|account| account.id == account_id)
            .ok_or_else(|| StoreError::InvalidConfig("git account not found".to_string()))?;
        account.status = GitAccountStatus::Verifying;
        account.last_error = None;
        let summary = account.summary(default_account_id.as_deref());
        self.save_git_accounts_config(&config).await?;
        Ok(summary)
    }

    pub async fn delete_git_account(&self, account_id: &str) -> Result<GitAccountsResponse> {
        let _guard = self.git_accounts_lock.lock().await;
        let mut config = self.git_accounts_config().await?;
        config.accounts.retain(|account| account.id != account_id);
        if config.default_account_id.as_deref() == Some(account_id) {
            config.default_account_id = config.accounts.first().map(|account| account.id.clone());
        }
        normalize_git_account_defaults(&mut config);
        self.save_git_accounts_config(&config).await?;
        Ok(git_accounts_response(&config))
    }

    pub async fn set_default_git_account(&self, account_id: &str) -> Result<GitAccountsResponse> {
        let _guard = self.git_accounts_lock.lock().await;
        let mut config = self.git_accounts_config().await?;
        if !config
            .accounts
            .iter()
            .any(|account| account.id == account_id)
        {
            return Err(StoreError::InvalidConfig(
                "git account not found".to_string(),
            ));
        }
        config.default_account_id = Some(account_id.to_string());
        normalize_git_account_defaults(&mut config);
        self.save_git_accounts_config(&config).await?;
        Ok(git_accounts_response(&config))
    }

    pub async fn git_account_token(&self, account_id: &str) -> Result<Option<String>> {
        let _guard = self.git_accounts_lock.lock().await;
        Ok(self
            .git_accounts_config()
            .await?
            .accounts
            .into_iter()
            .find(|account| account.id == account_id)
            .map(|account| account.token_secret))
    }

    pub async fn git_account(&self, account_id: &str) -> Result<Option<GitAccountSummary>> {
        let _guard = self.git_accounts_lock.lock().await;
        let config = self.git_accounts_config().await?;
        Ok(config
            .accounts
            .iter()
            .find(|account| account.id == account_id)
            .map(|account| account.summary(config.default_account_id.as_deref())))
    }

    async fn git_accounts_config(&self) -> Result<GitAccountsConfig> {
        let mut config = match self.get_setting(SETTING_GIT_ACCOUNTS).await? {
            Some(value) if !value.trim().is_empty() => serde_json::from_str(&value)?,
            _ => GitAccountsConfig::default(),
        };
        if config.accounts.is_empty()
            && let Some(token) = self.get_setting(SETTING_GITHUB_TOKEN).await?
        {
            let token = token.trim().to_string();
            if !token.is_empty() {
                config.accounts.push(StoredGitAccount {
                    id: "github-default".to_string(),
                    provider: GitProvider::Github,
                    label: "GitHub".to_string(),
                    login: None,
                    token_kind: GitTokenKind::Unknown,
                    scopes: Vec::new(),
                    status: GitAccountStatus::Unverified,
                    is_default: true,
                    token_secret: token,
                    last_verified_at: None,
                    last_error: None,
                    installation_id: None,
                    installation_account: None,
                    relay_id: None,
                });
                config.default_account_id = Some("github-default".to_string());
            }
        }
        normalize_git_account_defaults(&mut config);
        Ok(config)
    }

    async fn save_git_accounts_config(&self, config: &GitAccountsConfig) -> Result<()> {
        self.set_setting(SETTING_GIT_ACCOUNTS, &serde_json::to_string(config)?)
            .await
    }
}

impl StoredGitAccount {
    fn summary(&self, default_account_id: Option<&str>) -> GitAccountSummary {
        GitAccountSummary {
            id: self.id.clone(),
            provider: self.provider.clone(),
            label: self.label.clone(),
            login: self.login.clone(),
            token_kind: self.token_kind.clone(),
            scopes: self.scopes.clone(),
            status: self.status.clone(),
            is_default: default_account_id == Some(self.id.as_str()),
            has_token: !self.token_secret.trim().is_empty(),
            last_verified_at: self.last_verified_at,
            last_error: self.last_error.clone(),
            installation_id: self.installation_id,
            installation_account: self.installation_account.clone(),
            relay_id: self.relay_id.clone(),
        }
    }
}

fn git_accounts_response(config: &GitAccountsConfig) -> GitAccountsResponse {
    GitAccountsResponse {
        accounts: config
            .accounts
            .iter()
            .map(|account| account.summary(config.default_account_id.as_deref()))
            .collect(),
        default_account_id: config.default_account_id.clone(),
    }
}

fn normalize_git_account_defaults(config: &mut GitAccountsConfig) {
    if config.default_account_id.is_none() {
        config.default_account_id = config.accounts.first().map(|account| account.id.clone());
    }
    let default_account_id = config.default_account_id.clone();
    for account in &mut config.accounts {
        account.is_default = default_account_id.as_deref() == Some(account.id.as_str());
    }
}

trait StringDefault {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str;
}

impl StringDefault for str {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str {
        if self.is_empty() { fallback } else { self }
    }
}
