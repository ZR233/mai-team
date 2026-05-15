use crate::*;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RelayConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    node_id: Option<String>,
}

impl ConfigStore {
    pub async fn relay_settings(&self) -> Result<RelaySettingsResponse> {
        let config = self.relay_config().await?;
        Ok(relay_settings_response(&config))
    }

    pub async fn relay_secret(&self) -> Result<Option<(String, String, String)>> {
        let config = self.relay_config().await?;
        if !config.enabled {
            return Ok(None);
        }
        let token = config
            .token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let Some(token) = token else {
            return Ok(None);
        };
        let response = relay_settings_response(&config);
        Ok(Some((response.url, token, response.node_id)))
    }

    pub async fn save_relay_settings(
        &self,
        request: RelaySettingsRequest,
    ) -> Result<RelaySettingsResponse> {
        let mut current = self.relay_config().await?;
        current.enabled = request.enabled;
        current.url = request
            .url
            .map(|url| normalize_relay_url(&url))
            .filter(|value| !value.is_empty());
        if let Some(token) = request.token {
            current.token = Some(token.trim().to_string()).filter(|value| !value.is_empty());
        }
        current.node_id = request
            .node_id
            .map(|node_id| node_id.trim().to_string())
            .filter(|value| !value.is_empty());
        self.set_setting(SETTING_RELAY_CONFIG, &serde_json::to_string(&current)?)
            .await?;
        self.relay_settings().await
    }

    async fn relay_config(&self) -> Result<RelayConfig> {
        match self.get_setting(SETTING_RELAY_CONFIG).await? {
            Some(value) if !value.trim().is_empty() => Ok(serde_json::from_str(&value)?),
            _ => Ok(RelayConfig::default()),
        }
    }
}

fn relay_settings_response(config: &RelayConfig) -> RelaySettingsResponse {
    RelaySettingsResponse {
        enabled: config.enabled,
        url: config
            .url
            .as_deref()
            .map(normalize_relay_url)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_RELAY_URL.to_string()),
        has_token: config
            .token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty()),
        node_id: config
            .node_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_RELAY_NODE_ID)
            .to_string(),
    }
}

fn normalize_relay_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}
