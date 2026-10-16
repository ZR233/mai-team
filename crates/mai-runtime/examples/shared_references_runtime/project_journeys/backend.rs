//! 夹具 GitHub App backend：把安装、账户、仓库与 installation token 固定成本地夹具事实。
//!
//! 产品在装配 `AgentRuntime` 时允许注入 `Arc<dyn GithubAppBackend>`（`new_with_github_backend`）。
//! 验收用注入替换真实的 `DirectGithubAppBackend`：所有返回值都是假的（假 token、假安装），
//! 不访问网络、不创建 JWT、不发任何 GitHub 消息。仓库/PR 的 **HTTP** 读取仍由
//! `github_api_base_url` 指向的本地夹具提供，因此两半夹具互不混淆。

use std::sync::{Arc, Mutex};

use chrono::{TimeDelta, Utc};
use mai_protocol::{
    GithubAppManifestStartRequest, GithubAppManifestStartResponse, GithubAppSettingsRequest,
    GithubAppSettingsResponse, GithubInstallationSummary, GithubInstallationsResponse,
    GithubRepositoriesResponse, GithubRepositorySummary, RelayGithubInstallationTokenResponse,
};
use mai_runtime::RuntimeError;

use super::fixture::GitHubFixture;

/// 夹具 backend 的假 token 前缀：只用于本地容器内的 git/gh 调用，任何情况下都不出网。
const TOKEN_PREFIX: &str = "fixture-installation-token";

/// 返回静态夹具数据的 `GithubAppBackend`。
pub struct FixtureGithubAppBackend {
    installation_id: u64,
    account_login: String,
    repository_full_name: String,
    owner: String,
    name: String,
    default_branch: String,
    app_id: String,
    bot_login: String,
    bot_user_id: u64,
    fake_token: String,
    calls: Arc<Mutex<Vec<String>>>,
}

impl FixtureGithubAppBackend {
    /// 从夹具清单固化身份；清单里的 SHA 与此无关，只有安装/仓库/账户身份在这里使用。
    pub fn new(fixture: &GitHubFixture) -> Self {
        Self {
            installation_id: fixture.installation_id,
            account_login: fixture.installation_account.clone(),
            repository_full_name: fixture.repository_full_name.clone(),
            owner: fixture.owner.clone(),
            name: fixture.name.clone(),
            default_branch: fixture.default_branch.clone(),
            app_id: fixture.app_id.clone(),
            bot_login: fixture.app_bot_login.clone(),
            bot_user_id: fixture.app_bot_user_id,
            fake_token: if fixture.fake_token.trim().is_empty() {
                format!("{TOKEN_PREFIX}-{}", fixture.installation_id)
            } else {
                fixture.fake_token.clone()
            },
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 已发生的 backend 调用（调用顺序即验收证据），供证据落盘核对。
    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("fixture backend calls lock")
            .clone()
    }

    fn record(&self, operation: &str) {
        self.calls
            .lock()
            .expect("fixture backend calls lock")
            .push(operation.to_string());
    }

    fn settings(&self) -> GithubAppSettingsResponse {
        GithubAppSettingsResponse {
            app_id: Some(self.app_id.clone()),
            base_url: "http://127.0.0.1:18082".to_string(),
            public_url: None,
            has_private_key: false,
            app_slug: Some(self.name.clone()),
            github_name: Some(self.name.clone()),
            app_html_url: None,
            owner_login: Some(self.account_login.clone()),
            owner_type: Some("Organization".to_string()),
            bot_login: Some(self.bot_login.clone()),
            bot_user_id: Some(self.bot_user_id),
            install_url: None,
            manage_url: None,
        }
    }

    fn repository(&self) -> GithubRepositorySummary {
        GithubRepositorySummary {
            id: self.installation_id + 1,
            owner: self.owner.clone(),
            name: self.name.clone(),
            full_name: self.repository_full_name.clone(),
            private: false,
            clone_url: format!("https://github.com/{}.git", self.repository_full_name),
            html_url: format!("https://github.com/{}", self.repository_full_name),
            default_branch: Some(self.default_branch.clone()),
        }
    }
}

// 既有 dyn backend 接口返回 boxed Future；此测试适配器显式实现签名，不引入 async_trait 宏。
impl mai_runtime::github::GithubAppBackend for FixtureGithubAppBackend {
    fn github_app_settings<'life0, 'async_trait>(
        &'life0 self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = mai_runtime::Result<GithubAppSettingsResponse>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.record("github_app_settings");
            Ok(self.settings())
        })
    }

    fn refresh_github_app_settings<'life0, 'async_trait>(
        &'life0 self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = mai_runtime::Result<GithubAppSettingsResponse>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.record("refresh_github_app_settings");
            Ok(self.settings())
        })
    }

    fn save_github_app_settings<'life0, 'async_trait>(
        &'life0 self,
        _request: GithubAppSettingsRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = mai_runtime::Result<GithubAppSettingsResponse>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.record("save_github_app_settings");
            Err(RuntimeError::InvalidInput(
                "夹具 backend 不接受 GitHub App 设置写入".to_string(),
            ))
        })
    }

    fn start_github_app_manifest<'life0, 'async_trait>(
        &'life0 self,
        _request: GithubAppManifestStartRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = mai_runtime::Result<GithubAppManifestStartResponse>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.record("start_github_app_manifest");
            Err(RuntimeError::InvalidInput(
                "夹具 backend 不支持 GitHub App manifest 流程".to_string(),
            ))
        })
    }

    fn complete_github_app_manifest<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        _code: &'life1 str,
        _state: &'life2 str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = mai_runtime::Result<GithubAppSettingsResponse>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.record("complete_github_app_manifest");
            Err(RuntimeError::InvalidInput(
                "夹具 backend 不支持 GitHub App manifest 流程".to_string(),
            ))
        })
    }

    fn list_github_installations<'life0, 'async_trait>(
        &'life0 self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = mai_runtime::Result<GithubInstallationsResponse>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.record("list_github_installations");
            Ok(GithubInstallationsResponse {
                installations: vec![GithubInstallationSummary {
                    id: self.installation_id,
                    account_login: self.account_login.clone(),
                    account_type: "Organization".to_string(),
                    repository_selection: Some("all".to_string()),
                    events: vec!["pull_request".to_string()],
                }],
            })
        })
    }

    fn refresh_github_installations<'life0, 'async_trait>(
        &'life0 self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = mai_runtime::Result<GithubInstallationsResponse>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { self.list_github_installations().await })
    }

    fn list_github_repositories<'life0, 'async_trait>(
        &'life0 self,
        _installation_id: u64,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = mai_runtime::Result<GithubRepositoriesResponse>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.record("list_github_repositories");
            Ok(GithubRepositoriesResponse {
                repositories: vec![self.repository()],
            })
        })
    }

    fn get_github_repository<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _installation_id: u64,
        repository_full_name: &'life1 str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = mai_runtime::Result<GithubRepositorySummary>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.record("get_github_repository");
            if repository_full_name != self.repository_full_name {
                return Err(RuntimeError::InvalidInput(format!(
                    "夹具只登记仓库 {}，请求的是 {repository_full_name}",
                    self.repository_full_name
                )));
            }
            Ok(self.repository())
        })
    }

    fn github_installation_token<'life0, 'async_trait>(
        &'life0 self,
        installation_id: u64,
        _repository_id: Option<u64>,
        _include_packages: bool,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = mai_runtime::Result<RelayGithubInstallationTokenResponse>,
                > + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.record("github_installation_token");
            if installation_id != self.installation_id {
                return Err(RuntimeError::InvalidInput(format!(
                    "夹具只登记安装 {}，请求的是 {installation_id}",
                    self.installation_id
                )));
            }
            Ok(RelayGithubInstallationTokenResponse {
                token: self.fake_token.clone(),
                expires_at: Utc::now() + TimeDelta::hours(1),
            })
        })
    }
}
