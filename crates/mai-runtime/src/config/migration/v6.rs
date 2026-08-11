use std::collections::{BTreeMap, BTreeSet, HashMap};

use mai_store::ConfigDocumentStore;
use pl_core::{
    AgentModelConfig, AgentRoleId, ModelCatalogId, ModelRouteConfig, ProviderCapabilitySelection,
    ProviderConfig, ProviderId, ProviderModelCatalogConfig, ProviderPresetId, ReasoningEffort,
    builtin_model_catalog, builtin_provider_catalog,
};
use pl_model::{
    ApplyPatchToolType, ModelCapabilities, ModelInfo, ModelParameter, ModelRequestProfile,
    ModelTransportProfile, ProviderConnectionMode, ProviderWireProtocol, ToolWirePolicy,
    TruncationPolicy, WebSearchConfig,
};
use serde::{Deserialize, Serialize};

use super::{backup_document, set_supported_connection_mode};
use crate::config::{
    MAI_CONFIG_SCHEMA_VERSION, MaiConfig, MaiContainerConfig, MaiGithubConfig,
    MaiInstructionsConfig, MaiMcpConfig, MaiRetentionConfig, MaiReviewConfig, MaiSkillsConfig,
};
use crate::{Result, RuntimeError};

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct SchemaSixMaiConfig {
    pub(super) schema_version: u32,
    models: SchemaSixAgentModelConfig,
    #[serde(default)]
    web_search: WebSearchConfig,
    #[serde(default)]
    containers: MaiContainerConfig,
    #[serde(default)]
    instructions: MaiInstructionsConfig,
    #[serde(default)]
    skills: MaiSkillsConfig,
    #[serde(default)]
    mcp: MaiMcpConfig,
    #[serde(default)]
    github: MaiGithubConfig,
    #[serde(default)]
    review: MaiReviewConfig,
    #[serde(default)]
    retention: MaiRetentionConfig,
}

#[derive(Debug, Serialize, Deserialize)]
struct SchemaSixAgentModelConfig {
    providers: BTreeMap<ProviderId, SchemaSixProviderConfig>,
    routes: BTreeMap<AgentRoleId, SchemaSixModelRouteConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SchemaSixModelRouteConfig {
    provider: ProviderId,
    model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SchemaSixProviderConfig {
    transport: SchemaSixProviderTransport,
    name: String,
    base_url: String,
    #[serde(default)]
    bearer_token: Option<String>,
    #[serde(default)]
    bearer_token_env: Option<String>,
    #[serde(default)]
    http_headers: Option<HashMap<String, String>>,
    #[serde(default)]
    tool_wire_policy: ToolWirePolicy,
    #[serde(default)]
    apply_patch_tool_type: Option<ApplyPatchToolType>,
    #[serde(default)]
    capabilities: ProviderCapabilitySelection,
    catalog: SchemaSixProviderCatalog,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
enum SchemaSixProviderTransport {
    Preset {
        preset: ProviderPresetId,
        connection_mode: ProviderConnectionMode,
    },
    Custom {
        protocol: ProviderWireProtocol,
        connection_mode: ProviderConnectionMode,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
enum SchemaSixProviderCatalog {
    Bundled {
        catalog: ModelCatalogId,
        #[serde(default)]
        additional_models: Vec<SchemaSixModelInfo>,
    },
    Explicit {
        models: Vec<SchemaSixModelInfo>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct SchemaSixModelInfo {
    slug: String,
    display_name: String,
    description: Option<String>,
    context_window: Option<u64>,
    max_context_window: Option<u64>,
    auto_compact_token_limit: Option<u64>,
    default_temperature: Option<f32>,
    max_output_tokens: Option<u64>,
    currency: Option<String>,
    input_price_per_mtok: Option<f64>,
    output_price_per_mtok: Option<f64>,
    cache_read_price_per_mtok: Option<f64>,
    #[serde(default)]
    parameters: Vec<ModelParameter>,
    #[serde(default)]
    capabilities: ModelCapabilities,
    #[serde(default)]
    request_profile: ModelRequestProfile,
    #[serde(default)]
    truncation_policy: TruncationPolicy,
    #[serde(default)]
    base_instructions: String,
}

pub(super) async fn migrate(
    documents: &ConfigDocumentStore,
    legacy: SchemaSixMaiConfig,
) -> Result<MaiConfig> {
    let providers = legacy
        .models
        .providers
        .into_iter()
        .map(|(id, provider)| Ok((id, provider.migrate()?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let routes = legacy
        .models
        .routes
        .into_iter()
        .map(|(role, route)| {
            (
                role,
                ModelRouteConfig {
                    provider: route.provider,
                    model: route.model,
                    effort: route.reasoning_effort,
                },
            )
        })
        .collect();
    let config = MaiConfig {
        schema_version: MAI_CONFIG_SCHEMA_VERSION,
        models: AgentModelConfig { providers, routes },
        web_search: legacy.web_search,
        containers: legacy.containers,
        instructions: legacy.instructions,
        skills: legacy.skills,
        mcp: legacy.mcp,
        github: legacy.github,
        review: legacy.review,
        retention: legacy.retention,
    };
    config.validate()?;
    backup_document(documents.path(), 6).await?;
    documents.save(&config).await?;
    Ok(config)
}

impl SchemaSixProviderConfig {
    fn migrate(self) -> Result<ProviderConfig> {
        let (preset, protocol, connection_mode) = self.transport.resolve()?;
        let catalog = self.catalog.migrate(protocol)?;
        let mut provider = ProviderConfig {
            preset,
            name: self.name,
            base_url: self.base_url,
            bearer_token: self.bearer_token,
            bearer_token_env: self.bearer_token_env,
            http_headers: self.http_headers,
            tool_wire_policy: self.tool_wire_policy,
            apply_patch_tool_type: self.apply_patch_tool_type,
            capabilities: self.capabilities,
            catalog,
        };
        set_supported_connection_mode(&mut provider, connection_mode)?;
        Ok(provider)
    }
}

impl SchemaSixProviderTransport {
    fn resolve(
        self,
    ) -> Result<(
        Option<ProviderPresetId>,
        ProviderWireProtocol,
        ProviderConnectionMode,
    )> {
        match self {
            Self::Custom {
                protocol,
                connection_mode,
            } => Ok((None, protocol, connection_mode)),
            Self::Preset {
                preset,
                connection_mode,
            } => {
                let entry = builtin_provider_catalog()
                    .presets
                    .into_iter()
                    .find(|candidate| candidate.id == preset)
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput(format!(
                            "schema 6 配置引用未知 provider preset: {preset}"
                        ))
                    })?;
                let protocol = entry
                    .provider
                    .to_provider_info(&entry.suggested_model)
                    .map_err(RuntimeError::Model)?
                    .protocol;
                Ok((Some(preset), protocol, connection_mode))
            }
        }
    }
}

impl SchemaSixProviderCatalog {
    fn migrate(self, protocol: ProviderWireProtocol) -> Result<ProviderModelCatalogConfig> {
        match self {
            Self::Bundled {
                catalog,
                additional_models,
            } => {
                let bundled_slugs = builtin_model_catalog(&catalog)
                    .map_err(RuntimeError::Model)?
                    .models
                    .into_iter()
                    .map(|model| model.slug)
                    .collect::<BTreeSet<_>>();
                Ok(ProviderModelCatalogConfig::Bundled {
                    catalog,
                    additional_models: additional_models
                        .into_iter()
                        .filter(|model| !bundled_slugs.contains(&model.slug))
                        .map(|model| model.migrate(protocol))
                        .collect(),
                    connection_overrides: BTreeMap::new(),
                })
            }
            Self::Explicit { models } => Ok(ProviderModelCatalogConfig::Explicit {
                models: models
                    .into_iter()
                    .map(|model| model.migrate(protocol))
                    .collect(),
                connection_overrides: BTreeMap::new(),
            }),
        }
    }
}

impl SchemaSixModelInfo {
    fn migrate(self, protocol: ProviderWireProtocol) -> ModelInfo {
        let transport = match protocol {
            ProviderWireProtocol::Responses => ModelTransportProfile::responses_websocket(),
            ProviderWireProtocol::ChatCompletions => ModelTransportProfile::chat_completions_http(),
        };
        ModelInfo {
            slug: self.slug,
            display_name: self.display_name,
            description: self.description,
            context_window: self.context_window,
            max_context_window: self.max_context_window,
            auto_compact_token_limit: self.auto_compact_token_limit,
            default_temperature: self.default_temperature,
            max_output_tokens: self.max_output_tokens,
            currency: self.currency,
            input_price_per_mtok: self.input_price_per_mtok,
            output_price_per_mtok: self.output_price_per_mtok,
            cache_read_price_per_mtok: self.cache_read_price_per_mtok,
            cache_write_price_per_mtok: None,
            parameters: self.parameters,
            transport,
            capabilities: self.capabilities,
            request_profile: self.request_profile,
            truncation_policy: self.truncation_policy,
            base_instructions: self.base_instructions,
            used_fallback: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn schema_six_provider_transport_migrates_without_losing_private_fields() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        let documents = ConfigDocumentStore::new(path.clone());
        let provider_id = ProviderId::new("deepseek").expect("provider id");
        let provider = SchemaSixProviderConfig {
            transport: SchemaSixProviderTransport::Preset {
                preset: ProviderPresetId::new("deepseek").expect("preset id"),
                connection_mode: ProviderConnectionMode::Http,
            },
            name: "DeepSeek private".to_string(),
            base_url: "https://api.deepseek.com".to_string(),
            bearer_token: Some("secret".to_string()),
            bearer_token_env: Some("DEEPSEEK_API_KEY".to_string()),
            http_headers: Some(HashMap::from([(
                "X-Private".to_string(),
                "header-secret".to_string(),
            )])),
            tool_wire_policy: ToolWirePolicy::FunctionFallback,
            apply_patch_tool_type: None,
            capabilities: ProviderCapabilitySelection::PresetDefaults,
            catalog: SchemaSixProviderCatalog::Bundled {
                catalog: ModelCatalogId::new("deepseek").expect("catalog id"),
                additional_models: Vec::new(),
            },
        };
        let routes = ["planner", "explorer", "executor", "reviewer"]
            .into_iter()
            .map(|role| {
                (
                    AgentRoleId::new(role).expect("role id"),
                    SchemaSixModelRouteConfig {
                        provider: provider_id.clone(),
                        model: "deepseek-v4-flash".to_string(),
                        reasoning_effort: Some(ReasoningEffort::new("high")),
                    },
                )
            })
            .collect();
        let legacy = SchemaSixMaiConfig {
            schema_version: 6,
            models: SchemaSixAgentModelConfig {
                providers: BTreeMap::from([(provider_id.clone(), provider)]),
                routes,
            },
            web_search: WebSearchConfig::default(),
            containers: MaiContainerConfig::default(),
            instructions: MaiInstructionsConfig::default(),
            skills: MaiSkillsConfig::default(),
            mcp: MaiMcpConfig::default(),
            github: MaiGithubConfig::default(),
            review: MaiReviewConfig::default(),
            retention: MaiRetentionConfig::default(),
        };
        documents.save(&legacy).await.expect("save schema 6");

        let migrated = super::super::migrate(&documents)
            .await
            .expect("migrate schema 6");

        assert_eq!(migrated.schema_version, MAI_CONFIG_SCHEMA_VERSION);
        let migrated_provider = &migrated.models.providers[&provider_id];
        assert_eq!(migrated_provider.name, "DeepSeek private");
        assert_eq!(migrated_provider.bearer_token.as_deref(), Some("secret"));
        assert_eq!(
            migrated_provider.bearer_token_env.as_deref(),
            Some("DEEPSEEK_API_KEY")
        );
        assert_eq!(
            migrated_provider
                .http_headers
                .as_ref()
                .and_then(|headers| headers.get("X-Private"))
                .map(String::as_str),
            Some("header-secret")
        );
        assert_eq!(
            migrated_provider.preset_id().map(ToString::to_string),
            Some("deepseek".to_string())
        );
        assert_eq!(
            migrated.models.routes[&AgentRoleId::new("reviewer").expect("role id")]
                .effort
                .as_ref()
                .map(ReasoningEffort::as_str),
            Some("high")
        );
        migrated.validate().expect("validate migrated config");
        assert!(
            tokio::fs::try_exists(super::super::backup_path(&path, 6))
                .await
                .expect("backup lookup")
        );
        let persisted = documents
            .load::<MaiConfig>()
            .await
            .expect("load persisted config")
            .expect("persisted config");
        assert_eq!(persisted, migrated);
    }
}
