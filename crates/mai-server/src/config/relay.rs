use mai_relay_client::RelayClientConfig;

use super::Env;

#[derive(Debug, Clone)]
pub(crate) enum RelayMode {
    Disabled,
    Enabled(RelayClientConfig),
}

impl RelayMode {
    pub(crate) fn from_env(env: &impl Env) -> Self {
        let enabled = env
            .var("MAI_RELAY_ENABLED")
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "on"));
        if !enabled {
            return Self::Disabled;
        }

        let token = env.var("MAI_RELAY_TOKEN").unwrap_or_default();
        if token.trim().is_empty() {
            tracing::warn!("MAI_RELAY_ENABLED is set but MAI_RELAY_TOKEN is empty; relay disabled");
            return Self::Disabled;
        }

        let node_id = env
            .var("MAI_RELAY_NODE_ID")
            .unwrap_or_else(|| "mai-server".to_string());
        Self::Enabled(RelayClientConfig {
            url: relay_url_from_values(
                env.var("MAI_RELAY_PUBLIC_URL").as_deref(),
                env.var("MAI_RELAY_URL").as_deref(),
            ),
            token,
            node_id,
        })
    }
}

pub(crate) fn relay_url_from_values(public_url: Option<&str>, legacy_url: Option<&str>) -> String {
    public_url
        .or(legacy_url)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("http://127.0.0.1:8090")
        .trim_end_matches('/')
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::RelayMode;
    use crate::config::Env;

    #[derive(Default)]
    struct TestEnv {
        values: BTreeMap<&'static str, String>,
    }

    impl TestEnv {
        fn with(mut self, key: &'static str, value: &str) -> Self {
            self.values.insert(key, value.to_string());
            self
        }
    }

    impl Env for TestEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.values.get(key).cloned()
        }
    }

    #[test]
    fn relay_mode_is_disabled_when_not_enabled() {
        let env = TestEnv::default();

        assert!(matches!(RelayMode::from_env(&env), RelayMode::Disabled));
    }

    #[test]
    fn relay_mode_requires_token() {
        let env = TestEnv::default()
            .with("MAI_RELAY_ENABLED", "true")
            .with("MAI_RELAY_TOKEN", "  ");

        assert!(matches!(RelayMode::from_env(&env), RelayMode::Disabled));
    }

    #[test]
    fn relay_mode_prefers_public_url_and_trims_trailing_slash() {
        let env = TestEnv::default()
            .with("MAI_RELAY_ENABLED", "yes")
            .with("MAI_RELAY_TOKEN", "secret")
            .with("MAI_RELAY_NODE_ID", "node-a")
            .with("MAI_RELAY_PUBLIC_URL", "https://relay.example.com/")
            .with("MAI_RELAY_URL", "http://legacy.example.com/");

        let RelayMode::Enabled(config) = RelayMode::from_env(&env) else {
            panic!("relay should be enabled");
        };

        assert_eq!(config.url, "https://relay.example.com");
        assert_eq!(config.token, "secret");
        assert_eq!(config.node_id, "node-a");
    }
}
