use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;

use super::{Cli, Env, RelayMode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderSeedConfig {
    pub(crate) api_key: Option<String>,
    pub(crate) base_url: String,
    pub(crate) model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageConfig {
    pub(crate) agent_base_image: String,
    pub(crate) sidecar_image: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ServerConfig {
    pub(crate) bind_addr: SocketAddr,
    pub(crate) data_path: Option<PathBuf>,
    pub(crate) provider_seed: ProviderSeedConfig,
    pub(crate) images: ImageConfig,
    pub(crate) relay: RelayMode,
}

impl ServerConfig {
    pub(crate) fn from_sources(cli: Cli, env: &impl Env) -> anyhow::Result<Self> {
        let bind = env
            .var("MAI_BIND_ADDR")
            .unwrap_or_else(|| "0.0.0.0:8080".to_string());
        Ok(Self {
            bind_addr: bind.parse().context("invalid MAI_BIND_ADDR")?,
            data_path: cli.data_path,
            provider_seed: ProviderSeedConfig {
                api_key: env.var("OPENAI_API_KEY"),
                base_url: env
                    .var("OPENAI_BASE_URL")
                    .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
                model: env
                    .var("OPENAI_MODEL")
                    .unwrap_or_else(|| "gpt-5.5".to_string()),
            },
            images: ImageConfig {
                agent_base_image: env
                    .var("MAI_AGENT_BASE_IMAGE")
                    .unwrap_or_else(|| "ghcr.io/zr233/mai-team-agent:latest".to_string()),
                sidecar_image: env
                    .var("MAI_SIDECAR_IMAGE")
                    .unwrap_or_else(|| "ghcr.io/zr233/mai-team-sidecar:latest".to_string()),
            },
            relay: RelayMode::from_env(env),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::PathBuf;

    use crate::config::{Cli, Env, RelayMode, ServerConfig};

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
    fn uses_defaults_without_process_env_mutation() {
        let config = ServerConfig::from_sources(Cli { data_path: None }, &TestEnv::default())
            .expect("config");

        assert_eq!(
            config.bind_addr,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080)
        );
        assert_eq!(config.data_path, None);
        assert_eq!(config.provider_seed.api_key, None);
        assert_eq!(config.provider_seed.base_url, "https://api.openai.com/v1");
        assert_eq!(config.provider_seed.model, "gpt-5.5");
        assert_eq!(
            config.images.agent_base_image,
            "ghcr.io/zr233/mai-team-agent:latest"
        );
        assert_eq!(
            config.images.sidecar_image,
            "ghcr.io/zr233/mai-team-sidecar:latest"
        );
        assert!(matches!(config.relay, RelayMode::Disabled));
    }

    #[test]
    fn reads_values_from_injected_env() {
        let env = TestEnv::default()
            .with("OPENAI_API_KEY", "sk-test")
            .with("OPENAI_BASE_URL", "https://example.test/v1")
            .with("OPENAI_MODEL", "test-model")
            .with("MAI_AGENT_BASE_IMAGE", "agent:test")
            .with("MAI_SIDECAR_IMAGE", "sidecar:test")
            .with("MAI_BIND_ADDR", "127.0.0.1:9000")
            .with("MAI_RELAY_ENABLED", "1")
            .with("MAI_RELAY_TOKEN", "relay-token");

        let config = ServerConfig::from_sources(
            Cli {
                data_path: Some(PathBuf::from("/tmp/mai-data")),
            },
            &env,
        )
        .expect("config");

        assert_eq!(config.bind_addr, "127.0.0.1:9000".parse().unwrap());
        assert_eq!(config.data_path, Some(PathBuf::from("/tmp/mai-data")));
        assert_eq!(config.provider_seed.api_key, Some("sk-test".to_string()));
        assert_eq!(config.provider_seed.base_url, "https://example.test/v1");
        assert_eq!(config.provider_seed.model, "test-model");
        assert_eq!(config.images.agent_base_image, "agent:test");
        assert_eq!(config.images.sidecar_image, "sidecar:test");
        assert!(matches!(config.relay, RelayMode::Enabled(_)));
    }

    #[test]
    fn reports_invalid_bind_addr() {
        let env = TestEnv::default().with("MAI_BIND_ADDR", "not an addr");

        let error = ServerConfig::from_sources(Cli { data_path: None }, &env)
            .expect_err("invalid bind addr");

        assert!(error.to_string().contains("invalid MAI_BIND_ADDR"));
    }
}
