use std::env;

use mai_relay_client::RelayClientConfig;

pub(crate) fn relay_config_from_env() -> Option<RelayClientConfig> {
    let enabled = env::var("MAI_RELAY_ENABLED")
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "on"));
    if !enabled {
        return None;
    }
    let token = env::var("MAI_RELAY_TOKEN").unwrap_or_default();
    if token.trim().is_empty() {
        tracing::warn!("MAI_RELAY_ENABLED is set but MAI_RELAY_TOKEN is empty; relay disabled");
        return None;
    }
    let node_id = env::var("MAI_RELAY_NODE_ID").unwrap_or_else(|_| "mai-server".to_string());
    Some(RelayClientConfig {
        url: relay_url_from_env_values(
            env::var("MAI_RELAY_PUBLIC_URL").ok().as_deref(),
            env::var("MAI_RELAY_URL").ok().as_deref(),
        ),
        token,
        node_id,
    })
}

pub(crate) fn relay_url_from_env_values(
    public_url: Option<&str>,
    legacy_url: Option<&str>,
) -> String {
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
    use super::*;

    #[test]
    fn relay_url_prefers_public_url_and_trims_trailing_slash() {
        assert_eq!(
            relay_url_from_env_values(
                Some("https://relay.example.com/"),
                Some("http://legacy.example.com")
            ),
            "https://relay.example.com"
        );
        assert_eq!(
            relay_url_from_env_values(None, Some("http://legacy.example.com/")),
            "http://legacy.example.com"
        );
        assert_eq!(
            relay_url_from_env_values(Some("  "), None),
            "http://127.0.0.1:8090"
        );
    }
}
