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
    app_slug: Option<String>,
    #[serde(default)]
    app_html_url: Option<String>,
    #[serde(default)]
    owner_login: Option<String>,
    #[serde(default)]
    owner_type: Option<String>,
}

impl ConfigStore {
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
            app_html_url: config.app_html_url.clone(),
            owner_login: config.owner_login.clone(),
            owner_type: config.owner_type.clone(),
            install_url: github_app_install_url(config.app_slug.as_deref()),
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
        if let Some(private_key) = request.private_key {
            current.private_key =
                Some(private_key.trim().to_string()).filter(|value| !value.is_empty());
        }
        if let Some(base_url) = request.base_url {
            current.base_url = Some(base_url.trim().trim_end_matches('/').to_string())
                .filter(|value| !value.is_empty());
        }
        if let Some(public_url) = request.public_url {
            current.public_url = Some(public_url.trim().trim_end_matches('/').to_string())
                .filter(|value| !value.is_empty());
        }
        if let Some(app_slug) = request.app_slug {
            current.app_slug = Some(app_slug.trim().to_string()).filter(|value| !value.is_empty());
        }
        if let Some(app_html_url) = request.app_html_url {
            current.app_html_url =
                Some(app_html_url.trim().to_string()).filter(|value| !value.is_empty());
        }
        if let Some(owner_login) = request.owner_login {
            current.owner_login =
                Some(owner_login.trim().to_string()).filter(|value| !value.is_empty());
        }
        if let Some(owner_type) = request.owner_type {
            current.owner_type =
                Some(owner_type.trim().to_string()).filter(|value| !value.is_empty());
        }
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

fn github_app_install_url(app_slug: Option<&str>) -> Option<String> {
    app_slug
        .map(str::trim)
        .filter(|slug| !slug.is_empty())
        .map(|slug| format!("https://github.com/apps/{slug}/installations/select_target"))
}
