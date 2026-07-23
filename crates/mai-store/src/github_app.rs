use crate::*;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct GithubAppConfig {
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    private_key: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    public_url: Option<String>,
    #[serde(default)]
    github_name: Option<String>,
    #[serde(default)]
    app_slug: Option<String>,
    #[serde(default)]
    app_html_url: Option<String>,
    #[serde(default)]
    owner_login: Option<String>,
    #[serde(default)]
    owner_type: Option<String>,
    #[serde(default)]
    bot_login: Option<String>,
    #[serde(default)]
    bot_user_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubAppIdentity {
    pub github_name: String,
    pub app_slug: String,
    pub app_html_url: String,
    pub owner_login: Option<String>,
    pub owner_type: Option<String>,
    pub bot_login: String,
    pub bot_user_id: u64,
}

impl MaiStore {
    pub async fn get_github_app_settings(&self) -> Result<GithubAppSettingsResponse> {
        let config = self.github_app_config().await?;
        Ok(GithubAppSettingsResponse {
            app_id: config.app_id.clone(),
            base_url: config
                .base_url
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_GITHUB_API_BASE_URL.to_string()),
            public_url: config.public_url.clone(),
            has_private_key: config
                .private_key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty()),
            app_slug: config.app_slug.clone(),
            github_name: config.github_name.clone(),
            app_html_url: config.app_html_url.clone(),
            owner_login: config.owner_login.clone(),
            owner_type: config.owner_type.clone(),
            bot_login: config.bot_login.clone(),
            bot_user_id: config.bot_user_id,
            install_url: github_app_install_url(config.app_slug.as_deref()),
            manage_url: github_app_manage_url(
                config.app_slug.as_deref(),
                config.owner_login.as_deref(),
                config.owner_type.as_deref(),
            ),
        })
    }

    pub async fn github_app_secret(&self) -> Result<Option<(String, String, String)>> {
        let config = self.github_app_config().await?;
        let app_id = config.app_id.filter(|value| !value.trim().is_empty());
        let private_key = config.private_key.filter(|value| !value.trim().is_empty());
        match (app_id, private_key) {
            (Some(app_id), Some(private_key)) => Ok(Some((
                app_id,
                private_key,
                config
                    .base_url
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_GITHUB_API_BASE_URL.to_string()),
            ))),
            _ => Ok(None),
        }
    }

    pub async fn save_github_app_settings(
        &self,
        request: GithubAppSettingsRequest,
    ) -> Result<GithubAppSettingsResponse> {
        let mut current = self.github_app_config().await?;
        if let Some(app_id) = request.app_id {
            current.app_id = Some(app_id.trim().to_string()).filter(|value| !value.is_empty());
        }
        if let Some(private_key) = request
            .private_key
            .map(|private_key| private_key.trim().to_string())
            .filter(|private_key| !private_key.is_empty())
        {
            current.private_key = Some(private_key);
        }
        if let Some(base_url) = request.base_url {
            current.base_url = Some(base_url.trim().trim_end_matches('/').to_string())
                .filter(|value| !value.is_empty());
        }
        if let Some(public_url) = request.public_url {
            current.public_url = Some(public_url.trim().trim_end_matches('/').to_string())
                .filter(|value| !value.is_empty());
        }
        self.set_setting(SETTING_GITHUB_APP_CONFIG, &serde_json::to_string(&current)?)
            .await?;
        self.get_github_app_settings().await
    }

    pub async fn save_github_app_identity(
        &self,
        identity: GithubAppIdentity,
    ) -> Result<GithubAppSettingsResponse> {
        let mut current = self.github_app_config().await?;
        current.github_name = normalized_identity_text(identity.github_name);
        current.app_slug = normalized_identity_text(identity.app_slug);
        current.app_html_url = normalized_identity_text(identity.app_html_url);
        current.owner_login = identity.owner_login.and_then(normalized_identity_text);
        current.owner_type = identity.owner_type.and_then(normalized_identity_text);
        current.bot_login = normalized_identity_text(identity.bot_login);
        current.bot_user_id = Some(identity.bot_user_id);
        self.set_setting(SETTING_GITHUB_APP_CONFIG, &serde_json::to_string(&current)?)
            .await?;
        self.get_github_app_settings().await
    }

    async fn github_app_config(&self) -> Result<GithubAppConfig> {
        match self.get_setting(SETTING_GITHUB_APP_CONFIG).await? {
            Some(value) if !value.trim().is_empty() => Ok(serde_json::from_str(&value)?),
            _ => Ok(GithubAppConfig::default()),
        }
    }
}

fn normalized_identity_text(value: String) -> Option<String> {
    Some(value.trim().to_string()).filter(|value| !value.is_empty())
}

fn github_app_install_url(app_slug: Option<&str>) -> Option<String> {
    app_slug
        .map(str::trim)
        .filter(|slug| !slug.is_empty())
        .map(|slug| format!("https://github.com/apps/{slug}/installations/select_target"))
}

fn github_app_manage_url(
    app_slug: Option<&str>,
    owner_login: Option<&str>,
    owner_type: Option<&str>,
) -> Option<String> {
    let slug = app_slug.map(str::trim).filter(|value| !value.is_empty())?;
    match (
        owner_type.map(str::trim),
        owner_login.map(str::trim).filter(|value| !value.is_empty()),
    ) {
        (Some("Organization"), Some(owner)) => Some(format!(
            "https://github.com/organizations/{owner}/settings/apps/{slug}"
        )),
        _ => Some(format!("https://github.com/settings/apps/{slug}")),
    }
}
