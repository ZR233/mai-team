//! mai 产品配置；通过组合 PL 值对象形成独立 serde 文档。

mod conversion;
mod migration;

use std::collections::BTreeMap;

use pl_core::{
    AgentModelConfig, AgentRoleId, BuiltinMcpServerState, ModelRouteConfig, ProviderConfig,
    ProviderId, ReasoningEffort,
};
use pl_model::WebSearchConfig;
use serde::{Deserialize, Serialize};

use crate::{Result, RuntimeError};

pub use conversion::model_config_from_api;
pub(crate) use conversion::{
    agent_config_from_models, preserve_provider_private_fields, provider_selection_from_models,
    providers_request_from_models, providers_response_from_models,
};
pub const MAI_CONFIG_SCHEMA_VERSION: u32 = 5;

const REQUIRED_ROLES: [&str; 4] = ["planner", "explorer", "executor", "reviewer"];

/// mai 自己的完整配置文档。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MaiConfig {
    pub schema_version: u32,
    pub models: AgentModelConfig,
    #[serde(default)]
    pub web_search: WebSearchConfig,
    #[serde(default)]
    pub containers: MaiContainerConfig,
    #[serde(default)]
    pub instructions: MaiInstructionsConfig,
    #[serde(default)]
    pub skills: MaiSkillsConfig,
    #[serde(default)]
    pub mcp: MaiMcpConfig,
    #[serde(default)]
    pub github: MaiGithubConfig,
    #[serde(default)]
    pub review: MaiReviewConfig,
}

/// 容器与 turn 资源参数。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaiContainerConfig {
    pub turn_cancel_grace_ms: u64,
}

/// mai 产品提示词覆盖。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaiInstructionsConfig {
    #[serde(default)]
    pub base: String,
    #[serde(default)]
    pub developer: String,
    #[serde(default)]
    pub user: String,
}

/// mai skill 目录和启用规则。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaiSkillsConfig {
    pub enabled: bool,
    #[serde(default)]
    pub disabled: Vec<String>,
}

/// mai MCP 默认行为。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaiMcpConfig {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub builtin_servers: BTreeMap<String, BuiltinMcpServerState>,
}

/// GitHub 产品能力开关。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaiGithubConfig {
    pub api_base_url: String,
}

/// 自动审查产品参数。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaiReviewConfig {
    pub enabled_by_default: bool,
    pub max_concurrent_reviews: usize,
}

impl MaiConfig {
    /// 校验 schema、PL 模型引用和 mai 必需角色。
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != MAI_CONFIG_SCHEMA_VERSION {
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported mai config schema version: {}",
                self.schema_version
            )));
        }
        self.models.validate().map_err(RuntimeError::Model)?;
        pl_core::validate_builtin_mcp_server_states(&self.mcp.builtin_servers)
            .map_err(RuntimeError::Model)?;
        for role in REQUIRED_ROLES {
            let role = AgentRoleId::new(role).map_err(RuntimeError::Model)?;
            self.models.resolve(&role).map_err(RuntimeError::Model)?;
        }
        if self.containers.turn_cancel_grace_ms == 0 {
            return Err(RuntimeError::InvalidInput(
                "turn_cancel_grace_ms must be greater than zero".to_string(),
            ));
        }
        if self.review.max_concurrent_reviews == 0 {
            return Err(RuntimeError::InvalidInput(
                "max_concurrent_reviews must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

pub(crate) async fn load_or_initialize(store: &mai_store::MaiStore) -> Result<MaiConfig> {
    let documents = store.config_documents();
    match documents.load::<MaiConfig>().await {
        Ok(Some(config)) if config.validate().is_ok() => Ok(config),
        Ok(None) => {
            let config = MaiConfig::default();
            config.validate()?;
            documents.save(&config).await?;
            Ok(config)
        }
        Ok(Some(_)) | Err(_) => migration::migrate(&documents).await,
    }
}

pub(crate) async fn save(store: &mai_store::MaiStore, config: &MaiConfig) -> Result<()> {
    config.validate()?;
    store.config_documents().save(config).await?;
    Ok(())
}

/// 首次启动时把环境变量 provider seed 写入完整 MaiConfig 文档。
pub async fn seed_default_provider_from_env(
    store: &mai_store::MaiStore,
    api_key: Option<String>,
    base_url: String,
    model: String,
) -> Result<()> {
    let documents = store.config_documents();
    if let Ok(Some(config)) = documents.load::<MaiConfig>().await
        && config.validate().is_ok()
    {
        return Ok(());
    }
    let Some(api_key) = api_key.filter(|value| !value.trim().is_empty()) else {
        return Ok(());
    };
    let api_provider = mai_protocol::ProviderConfig {
        id: "openai".to_string(),
        preset_id: None,
        transport: mai_protocol::ProviderTransportConfig {
            protocol: mai_protocol::ProviderWireProtocol::Responses,
            connection_mode: mai_protocol::ProviderConnectionMode::Http,
        },
        capabilities: mai_protocol::ProviderCapabilitySelection::Explicit(Default::default()),
        name: "OpenAI".to_string(),
        base_url,
        api_key: Some(api_key),
        api_key_env: None,
        http_headers: None,
        catalog: mai_protocol::ProviderModelCatalogConfig::Explicit {
            models: vec![seed_model(&model)],
        },
        default_model: model.clone(),
        enabled: true,
    };
    let preference = mai_protocol::AgentModelPreference {
        provider_id: "openai".to_string(),
        model,
        reasoning_effort: None,
    };
    let models = model_config_from_api(
        &mai_protocol::ProvidersConfigRequest {
            providers: vec![api_provider],
            default_provider_id: Some("openai".to_string()),
        },
        &mai_protocol::AgentConfigRequest {
            planner: Some(preference.clone()),
            explorer: Some(preference.clone()),
            executor: Some(preference.clone()),
            reviewer: Some(preference),
        },
    )?;
    let config = MaiConfig {
        models,
        ..MaiConfig::default()
    };
    save(store, &config).await
}

/// 返回 PL canonical provider/model catalog，供 HTTP 与其它 mai 产品边界直接透传。
pub fn provider_catalog_snapshot() -> Result<pl_protocol::ProviderCatalogSnapshot> {
    pl_core::builtin_provider_catalog()
        .snapshot()
        .map_err(RuntimeError::Model)
}

fn seed_model(id: &str) -> mai_protocol::ModelConfig {
    mai_protocol::ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens: 128_000,
        max_context_tokens: None,
        effective_context_window_percent: 95,
        output_tokens: 8_192,
        auto_compact_token_limit: None,
        supports_tools: true,
        capabilities: mai_protocol::ModelCapabilities::default(),
        request_policy: mai_protocol::ModelRequestPolicy::default(),
        reasoning: None,
        options: serde_json::Value::Null,
        headers: BTreeMap::new(),
    }
}

impl Default for MaiConfig {
    fn default() -> Self {
        let provider_id = ProviderId::new("deepseek").expect("static provider id is valid");
        let provider = ProviderConfig::deepseek_preset();
        let route = ModelRouteConfig {
            provider: provider_id.clone(),
            model: "deepseek-v4-flash".to_string(),
            reasoning_effort: Some(ReasoningEffort::new("high")),
        };
        let routes = REQUIRED_ROLES
            .into_iter()
            .map(|role| {
                (
                    AgentRoleId::new(role).expect("static role id is valid"),
                    route.clone(),
                )
            })
            .collect();
        Self {
            schema_version: MAI_CONFIG_SCHEMA_VERSION,
            models: AgentModelConfig {
                providers: BTreeMap::from([(provider_id, provider)]),
                routes,
            },
            web_search: WebSearchConfig::default(),
            containers: MaiContainerConfig::default(),
            instructions: MaiInstructionsConfig::default(),
            skills: MaiSkillsConfig::default(),
            mcp: MaiMcpConfig::default(),
            github: MaiGithubConfig::default(),
            review: MaiReviewConfig::default(),
        }
    }
}

impl Default for MaiContainerConfig {
    fn default() -> Self {
        Self {
            turn_cancel_grace_ms: 500,
        }
    }
}

impl Default for MaiSkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            disabled: Vec::new(),
        }
    }
}

impl Default for MaiMcpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            builtin_servers: BTreeMap::new(),
        }
    }
}

impl Default for MaiGithubConfig {
    fn default() -> Self {
        Self {
            api_base_url: "https://api.github.com".to_string(),
        }
    }
}

impl Default for MaiReviewConfig {
    fn default() -> Self {
        Self {
            enabled_by_default: false,
            max_concurrent_reviews: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn mai_config_round_trips_through_generic_document_store() {
        let directory = tempfile::tempdir().unwrap();
        let store = mai_store::ConfigDocumentStore::new(directory.path().join("config.toml"));
        let config = MaiConfig::default();

        store.save(&config).await.unwrap();
        let restored: MaiConfig = store.load().await.unwrap().unwrap();

        assert_eq!(restored, config);
        restored.validate().unwrap();
    }

    #[test]
    fn product_validation_requires_all_mai_roles() {
        let mut config = MaiConfig::default();
        config
            .models
            .routes
            .remove(&AgentRoleId::new("reviewer").unwrap());

        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("reviewer")
        );
    }
}
