use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mai_store::ConfigDocumentStore;
use pl_core::{
    AgentModelConfig, AgentRoleId, ModelRouteConfig, ProviderCapabilitySelection, ProviderConfig,
    ProviderId, ProviderModelCatalogConfig, ProviderPresetId, ReasoningEffort,
    builtin_provider_catalog,
};
use pl_model::{
    ApplyPatchToolType, ModelInfo, ProviderConnectionMode, ProviderInfo, ProviderWireProtocol,
    ToolWirePolicy,
};
use serde::{Deserialize, Serialize};

use super::{
    MAI_CONFIG_SCHEMA_VERSION, MaiConfig, MaiContainerConfig, MaiGithubConfig,
    MaiInstructionsConfig, MaiMcpConfig, MaiRetentionConfig, MaiReviewConfig, MaiSkillsConfig,
};
use crate::{Result, RuntimeError};

mod v6;
mod v8;

#[derive(Debug, Deserialize)]
struct LegacyMaiConfig {
    schema_version: u32,
    models: LegacyAgentModelConfig,
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
}

#[derive(Debug, Deserialize)]
struct LegacyAgentModelConfig {
    providers: BTreeMap<ProviderId, LegacyProviderConfig>,
    routes: BTreeMap<AgentRoleId, ModelRouteConfig>,
}

#[derive(Debug, Deserialize)]
struct LegacyProviderConfig {
    provider_kind: LegacyProviderKind,
    name: String,
    base_url: String,
    #[serde(default)]
    bearer_token: Option<String>,
    #[serde(default)]
    bearer_token_env: Option<String>,
    #[serde(default)]
    http_headers: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    tool_wire_policy: ToolWirePolicy,
    #[serde(default)]
    apply_patch_tool_type: Option<ApplyPatchToolType>,
    models: Vec<ModelInfo>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LegacyProviderKind {
    #[serde(alias = "openai")]
    OpenAi,
    OpenAiCompatibleChat,
    #[serde(alias = "deepseek")]
    DeepSeek,
    Zhipu,
}

#[derive(Debug, Deserialize)]
struct LegacyCatalogMaiConfig {
    schema_version: u32,
    models: LegacyCatalogAgentModelConfig,
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
}

#[derive(Debug, Deserialize)]
struct LegacyCatalogAgentModelConfig {
    providers: BTreeMap<ProviderId, LegacyCatalogProviderConfig>,
    routes: BTreeMap<AgentRoleId, ModelRouteConfig>,
}

#[derive(Debug, Deserialize)]
struct LegacyCatalogProviderConfig {
    #[serde(default, alias = "preset_id")]
    preset: Option<ProviderPresetId>,
    #[serde(rename = "provider_kind")]
    _provider_kind: LegacyProviderKind,
    #[serde(default)]
    connection_mode: Option<ProviderConnectionMode>,
    name: String,
    base_url: String,
    #[serde(default)]
    bearer_token: Option<String>,
    #[serde(default)]
    bearer_token_env: Option<String>,
    #[serde(default)]
    http_headers: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    tool_wire_policy: ToolWirePolicy,
    #[serde(default)]
    apply_patch_tool_type: Option<ApplyPatchToolType>,
    catalog: ProviderModelCatalogConfig,
}

pub(super) async fn migrate(documents: &ConfigDocumentStore) -> Result<MaiConfig> {
    if let Ok(Some(legacy)) = documents.load::<v6::SchemaSixMaiConfig>().await
        && legacy.schema_version == 6
    {
        return v6::migrate(documents, legacy).await;
    }
    if let Ok(Some(legacy)) = documents.load::<v8::LegacyCurrentMaiConfig>().await
        && matches!(legacy.schema_version, 4 | 5 | 7 | 8)
    {
        return v8::migrate(documents, legacy).await;
    }
    if let Ok(Some(legacy)) = documents.load::<LegacyCatalogMaiConfig>().await
        && matches!(legacy.schema_version, 2 | 3)
    {
        return migrate_catalog_config(documents, legacy).await;
    }
    migrate_v1(documents).await
}

fn migrate_v4_provider_capabilities(models: &mut AgentModelConfig) {
    for provider in models.providers.values_mut() {
        provider.capabilities = if provider.preset.is_some() {
            ProviderCapabilitySelection::PresetDefaults
        } else {
            ProviderCapabilitySelection::Explicit(Default::default())
        };
    }
}

fn canonicalize_preset_providers(models: &mut AgentModelConfig) -> Result<()> {
    let registry = builtin_provider_catalog();
    for provider in models.providers.values_mut() {
        let Some(preset_id) = provider.preset.clone() else {
            continue;
        };
        let preset = registry
            .presets
            .iter()
            .find(|candidate| candidate.id == preset_id)
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "schema 7 配置引用未知 provider preset: {preset_id}"
                ))
            })?;
        let mut canonical = preset.provider.clone();
        canonical.name.clone_from(&provider.name);
        canonical.base_url.clone_from(&provider.base_url);
        canonical.bearer_token.clone_from(&provider.bearer_token);
        canonical
            .bearer_token_env
            .clone_from(&provider.bearer_token_env);
        canonical.http_headers.clone_from(&provider.http_headers);
        *provider = canonical;
    }
    Ok(())
}

async fn migrate_catalog_config(
    documents: &ConfigDocumentStore,
    legacy: LegacyCatalogMaiConfig,
) -> Result<MaiConfig> {
    let previous_version = legacy.schema_version;
    let providers = legacy
        .models
        .providers
        .into_iter()
        .map(|(id, provider)| Ok((id, migrate_catalog_provider(previous_version, provider)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut config = MaiConfig {
        schema_version: MAI_CONFIG_SCHEMA_VERSION,
        models: AgentModelConfig {
            providers,
            routes: legacy.models.routes,
        },
        web_search: Default::default(),
        containers: legacy.containers,
        instructions: legacy.instructions,
        skills: legacy.skills,
        mcp: legacy.mcp,
        github: legacy.github,
        review: legacy.review,
        retention: MaiRetentionConfig::default(),
    };
    canonicalize_preset_providers(&mut config.models)?;
    config.validate()?;
    backup_document(documents.path(), previous_version).await?;
    documents.save(&config).await?;
    Ok(config)
}

fn migrate_catalog_provider(
    schema_version: u32,
    legacy: LegacyCatalogProviderConfig,
) -> Result<ProviderConfig> {
    let connection_mode = if schema_version == 2 {
        migrated_preset_connection_mode(legacy.preset.as_ref(), &legacy.base_url)
    } else {
        legacy.connection_mode.unwrap_or_else(|| {
            migrated_preset_connection_mode(legacy.preset.as_ref(), &legacy.base_url)
        })
    };
    let capabilities = if legacy.preset.is_some() {
        ProviderCapabilitySelection::PresetDefaults
    } else {
        ProviderCapabilitySelection::Explicit(Default::default())
    };
    let mut provider = ProviderConfig {
        preset: legacy.preset,
        name: legacy.name,
        base_url: legacy.base_url,
        bearer_token: legacy.bearer_token,
        bearer_token_env: legacy.bearer_token_env,
        http_headers: legacy.http_headers,
        tool_wire_policy: legacy.tool_wire_policy,
        apply_patch_tool_type: legacy.apply_patch_tool_type,
        capabilities,
        catalog: legacy.catalog,
    };
    set_supported_connection_mode(&mut provider, connection_mode)?;
    Ok(provider)
}

async fn migrate_v1(documents: &ConfigDocumentStore) -> Result<MaiConfig> {
    let legacy = documents
        .load::<LegacyMaiConfig>()
        .await?
        .ok_or_else(|| RuntimeError::InvalidInput("mai config document is missing".to_string()))?;
    if legacy.schema_version != 1 {
        return Err(RuntimeError::InvalidInput(format!(
            "unsupported mai config schema version: {}",
            legacy.schema_version
        )));
    }

    let mut routes = legacy.models.routes;
    migrate_deprecated_mimo_routes(&mut routes);
    let providers = legacy
        .models
        .providers
        .into_iter()
        .map(|(id, provider)| Ok((id.clone(), migrate_provider(&id, provider)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut config = MaiConfig {
        schema_version: MAI_CONFIG_SCHEMA_VERSION,
        models: AgentModelConfig { providers, routes },
        web_search: Default::default(),
        containers: legacy.containers,
        instructions: legacy.instructions,
        skills: legacy.skills,
        mcp: legacy.mcp,
        github: legacy.github,
        review: legacy.review,
        retention: MaiRetentionConfig::default(),
    };
    canonicalize_preset_providers(&mut config.models)?;
    config.validate()?;
    backup_document(documents.path(), 1).await?;
    documents.save(&config).await?;
    Ok(config)
}

fn migrate_provider(id: &ProviderId, legacy: LegacyProviderConfig) -> Result<ProviderConfig> {
    let info = ProviderInfo {
        protocol: legacy.provider_kind.protocol(),
        connection_mode: ProviderConnectionMode::Http,
        name: legacy.name,
        base_url: legacy.base_url,
        default_model: String::new(),
        bearer_token: legacy.bearer_token,
        http_headers: legacy.http_headers,
        tool_wire_policy: legacy.tool_wire_policy,
        apply_patch_tool_type: legacy.apply_patch_tool_type,
        service_capabilities: Default::default(),
    };
    let bearer_token_env = legacy.bearer_token_env;
    let registry = builtin_provider_catalog();
    let preset = registry.presets.into_iter().find(|preset| {
        preset.id.as_str() == id.as_str()
            || (preset
                .provider
                .to_provider_info(&preset.suggested_model)
                .is_ok_and(|provider| provider.protocol == info.protocol)
                && normalized_url(&preset.provider.base_url) == normalized_url(&info.base_url))
    });
    let Some(preset) = preset else {
        let mut provider = ProviderConfig::from_explicit_models(info, legacy.models);
        provider.bearer_token_env = bearer_token_env;
        return Ok(provider);
    };
    let bundled = preset
        .provider
        .effective_models()
        .map_err(RuntimeError::Model)?;
    let bundled_slugs = bundled
        .iter()
        .map(|model| model.slug.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let additional_models = legacy
        .models
        .into_iter()
        .filter(|model| model.slug != "mimo-v2-flash")
        .filter(|model| !bundled_slugs.contains(model.slug.as_str()))
        .collect::<Vec<_>>();
    let connection_mode =
        if normalized_url(&preset.provider.base_url) == normalized_url(&info.base_url) {
            preset
                .provider
                .to_provider_info(&preset.suggested_model)
                .map_err(RuntimeError::Model)?
                .connection_mode
        } else {
            ProviderConnectionMode::Http
        };
    let mut provider = preset.provider;
    provider.name = info.name;
    provider.base_url = info.base_url;
    provider.bearer_token = info.bearer_token;
    provider.bearer_token_env = bearer_token_env.or(provider.bearer_token_env);
    provider.http_headers = info.http_headers;
    provider.tool_wire_policy = info.tool_wire_policy;
    provider.apply_patch_tool_type = info.apply_patch_tool_type;
    if let ProviderModelCatalogConfig::Bundled {
        additional_models: configured,
        ..
    } = &mut provider.catalog
    {
        *configured = additional_models;
    }
    set_supported_connection_mode(&mut provider, connection_mode)?;
    Ok(provider)
}

fn migrate_deprecated_mimo_routes(routes: &mut BTreeMap<AgentRoleId, ModelRouteConfig>) {
    for route in routes.values_mut() {
        if route.model != "mimo-v2-flash" {
            continue;
        }
        route.model = "mimo-v2.5".to_string();
        route.effort = route.effort.take().map(|effort| {
            let migrated = match effort.as_str() {
                "none" | "disabled" => "disabled",
                "high" | "max" | "medium" | "low" | "enabled" => "enabled",
                value => value,
            };
            ReasoningEffort::new(migrated)
        });
    }
}

pub(super) async fn backup_document(path: &Path, version: u32) -> Result<()> {
    let backup = backup_path(path, version);
    if !tokio::fs::try_exists(&backup).await? {
        tokio::fs::copy(path, backup).await?;
    }
    Ok(())
}

fn backup_path(path: &Path, version: u32) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    path.with_file_name(format!("{file_name}.schema{version}.bak"))
}

impl LegacyProviderKind {
    fn protocol(self) -> ProviderWireProtocol {
        match self {
            Self::OpenAi => ProviderWireProtocol::Responses,
            Self::OpenAiCompatibleChat | Self::DeepSeek | Self::Zhipu => {
                ProviderWireProtocol::ChatCompletions
            }
        }
    }
}

fn migrated_preset_connection_mode(
    preset: Option<&ProviderPresetId>,
    base_url: &str,
) -> ProviderConnectionMode {
    let Some(preset) = preset else {
        return ProviderConnectionMode::Http;
    };
    builtin_provider_catalog()
        .presets
        .into_iter()
        .find(|candidate| &candidate.id == preset)
        .filter(|candidate| {
            normalized_url(&candidate.provider.base_url) == normalized_url(base_url)
        })
        .and_then(|candidate| {
            candidate
                .provider
                .to_provider_info(&candidate.suggested_model)
                .ok()
                .map(|provider| provider.connection_mode)
        })
        .unwrap_or(ProviderConnectionMode::Http)
}

pub(super) fn set_supported_connection_mode(
    provider: &mut ProviderConfig,
    mode: ProviderConnectionMode,
) -> Result<()> {
    let models = provider.declared_models().map_err(RuntimeError::Model)?;
    for model in models {
        if model.transport.supported_connection_modes.contains(&mode) {
            provider
                .set_model_connection_mode(&model.slug, mode)
                .map_err(RuntimeError::Model)?;
        }
    }
    Ok(())
}

fn normalized_url(value: &str) -> &str {
    value.trim().trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn deprecated_mimo_flash_route_maps_to_v25_and_thinking_values() {
        let role = AgentRoleId::new("executor").unwrap();
        let mut routes = BTreeMap::from([(
            role.clone(),
            ModelRouteConfig {
                provider: ProviderId::new("mimo-token-plan").unwrap(),
                model: "mimo-v2-flash".to_string(),
                effort: Some(ReasoningEffort::new("max")),
            },
        )]);

        migrate_deprecated_mimo_routes(&mut routes);

        assert_eq!(routes[&role].model, "mimo-v2.5");
        assert_eq!(routes[&role].effort.as_ref().unwrap().as_str(), "enabled");
    }

    #[test]
    fn schema_four_capabilities_follow_preset_but_not_custom_provider() {
        let mut models = crate::MaiConfig::default().models;
        let custom_id = ProviderId::new("custom").unwrap();
        let custom = ProviderConfig::from_explicit_models(
            ProviderInfo::responses_compatible("Custom", "https://example.com/v1", "future-model"),
            vec![ModelInfo::fallback("future-model")],
        );
        models.providers.insert(custom_id.clone(), custom);
        for provider in models.providers.values_mut() {
            provider.capabilities = ProviderCapabilitySelection::PresetDefaults;
        }

        migrate_v4_provider_capabilities(&mut models);

        assert!(matches!(
            models.providers[&ProviderId::new("deepseek").unwrap()].capabilities,
            ProviderCapabilitySelection::PresetDefaults
        ));
        assert!(matches!(
            models.providers[&custom_id].capabilities,
            ProviderCapabilitySelection::Explicit(_)
        ));
    }

    #[tokio::test]
    async fn schema_five_adds_default_retention_and_creates_backup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let documents = ConfigDocumentStore::new(path.clone());
        let mut value = toml::Value::try_from(MaiConfig::default()).unwrap();
        let table = value.as_table_mut().unwrap();
        table.insert("schema_version".to_string(), toml::Value::Integer(5));
        table.remove("retention");
        tokio::fs::write(&path, toml::to_string_pretty(&value).unwrap())
            .await
            .unwrap();

        let migrated = migrate(&documents).await.unwrap();

        assert_eq!(MAI_CONFIG_SCHEMA_VERSION, migrated.schema_version);
        assert_eq!(MaiRetentionConfig::default(), migrated.retention);
        assert!(tokio::fs::try_exists(backup_path(&path, 5)).await.unwrap());
    }

    #[tokio::test]
    async fn schema_seven_reloads_preset_semantics_and_preserves_custom_provider() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let documents = ConfigDocumentStore::new(path.clone());
        let mut config = v8::legacy_current_config(7);
        let preset_id = ProviderId::new("deepseek").unwrap();
        let preset = config.models.providers.get_mut(&preset_id).unwrap();
        preset.name = "DeepSeek proxy".to_string();
        preset.base_url = "https://proxy.example/v1".to_string();
        preset.bearer_token = Some("secret".to_string());
        preset.http_headers = Some(std::collections::HashMap::from([(
            "x-project".to_string(),
            "mai".to_string(),
        )]));
        preset.tool_wire_policy = ToolWirePolicy::NativeCustomTools;
        preset.catalog = ProviderModelCatalogConfig::Explicit {
            models: vec![ModelInfo::fallback("fake-model")],
            connection_overrides: BTreeMap::new(),
        };
        let expected_headers = preset.http_headers.clone();
        let custom_id = ProviderId::new("custom").unwrap();
        let mut custom_model = ModelInfo::fallback("custom");
        custom_model.used_fallback = false;
        let custom = ProviderConfig::from_explicit_models(
            ProviderInfo::responses_compatible("Custom", "https://custom.example/v1", "custom"),
            vec![custom_model],
        );
        config
            .models
            .providers
            .insert(custom_id.clone(), custom.clone());
        documents.save(&config).await.unwrap();

        let migrated = migrate(&documents).await.unwrap();

        let migrated_preset = &migrated.models.providers[&preset_id];
        let canonical = builtin_provider_catalog()
            .presets
            .into_iter()
            .find(|preset| preset.id.as_str() == "deepseek")
            .unwrap()
            .provider;
        assert_eq!(migrated_preset.name, "DeepSeek proxy");
        assert_eq!(migrated_preset.base_url, "https://proxy.example/v1");
        assert_eq!(migrated_preset.bearer_token.as_deref(), Some("secret"));
        assert_eq!(migrated_preset.http_headers, expected_headers);
        assert_eq!(migrated_preset.tool_wire_policy, canonical.tool_wire_policy);
        assert_eq!(migrated_preset.catalog, canonical.catalog);
        assert_eq!(migrated.models.providers[&custom_id], custom);
        assert!(tokio::fs::try_exists(backup_path(&path, 7)).await.unwrap());
    }

    #[tokio::test]
    async fn schema_seven_unknown_preset_leaves_document_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let documents = ConfigDocumentStore::new(path.clone());
        let mut config = v8::legacy_current_config(7);
        config.models.providers.values_mut().next().unwrap().preset =
            Some(ProviderPresetId::new("future-preset").unwrap());
        documents.save(&config).await.unwrap();
        let before = tokio::fs::read(&path).await.unwrap();

        let error = migrate(&documents).await.unwrap_err();

        assert!(error.to_string().contains("future-preset"));
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        assert!(!tokio::fs::try_exists(backup_path(&path, 7)).await.unwrap());
    }

    #[test]
    fn schema_two_defaults_only_canonical_openai_endpoint_to_websocket() {
        let preset = migrate_catalog_provider(
            2,
            legacy_catalog_provider(Some("openai"), Some(ProviderConnectionMode::Http)),
        )
        .unwrap();
        let custom = migrate_catalog_provider(2, legacy_catalog_provider(None, None)).unwrap();

        assert_eq!(connection_mode(&preset), ProviderConnectionMode::Http);
        assert_eq!(connection_mode(&custom), ProviderConnectionMode::Http);
    }

    #[test]
    fn schema_two_keeps_openai_preset_endpoint_override_on_http() {
        let mut legacy =
            legacy_catalog_provider(Some("openai"), Some(ProviderConnectionMode::WebSocket));
        legacy.base_url = "https://ai.muxai.net".to_string();

        let provider = migrate_catalog_provider(2, legacy).unwrap();

        assert_eq!(connection_mode(&provider), ProviderConnectionMode::Http);
    }

    #[test]
    fn schema_three_preserves_explicit_http_for_openai_preset() {
        let provider = migrate_catalog_provider(
            3,
            legacy_catalog_provider(Some("openai"), Some(ProviderConnectionMode::Http)),
        )
        .unwrap();

        assert_eq!(connection_mode(&provider), ProviderConnectionMode::Http);
    }

    #[test]
    fn schema_three_preserves_explicit_websocket_for_endpoint_override() {
        let mut legacy =
            legacy_catalog_provider(Some("openai"), Some(ProviderConnectionMode::WebSocket));
        legacy.base_url = "https://ai.muxai.net".to_string();

        let provider = migrate_catalog_provider(3, legacy).unwrap();

        assert_eq!(connection_mode(&provider), ProviderConnectionMode::Http);
    }

    #[test]
    fn schema_one_openai_id_with_endpoint_override_stays_on_http() {
        let provider = migrate_provider(
            &ProviderId::new("openai").unwrap(),
            LegacyProviderConfig {
                provider_kind: LegacyProviderKind::OpenAi,
                name: "OpenAI compatible".to_string(),
                base_url: "https://ai.muxai.net".to_string(),
                bearer_token: Some("secret".to_string()),
                bearer_token_env: None,
                http_headers: None,
                tool_wire_policy: ToolWirePolicy::NativeCustomTools,
                apply_patch_tool_type: Some(ApplyPatchToolType::Freeform),
                models: vec![ModelInfo::fallback("future-model")],
            },
        )
        .unwrap();

        assert_eq!(connection_mode(&provider), ProviderConnectionMode::Http);
    }

    fn legacy_catalog_provider(
        preset: Option<&str>,
        connection_mode: Option<ProviderConnectionMode>,
    ) -> LegacyCatalogProviderConfig {
        LegacyCatalogProviderConfig {
            preset: preset.map(|id| ProviderPresetId::new(id).unwrap()),
            _provider_kind: LegacyProviderKind::OpenAi,
            connection_mode,
            name: "OpenAI".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            bearer_token: Some("secret".to_string()),
            bearer_token_env: None,
            http_headers: None,
            tool_wire_policy: ToolWirePolicy::NativeCustomTools,
            apply_patch_tool_type: Some(ApplyPatchToolType::Freeform),
            catalog: ProviderModelCatalogConfig::Explicit {
                models: vec![ModelInfo::fallback("model")],
                connection_overrides: BTreeMap::new(),
            },
        }
    }

    fn connection_mode(provider: &ProviderConfig) -> ProviderConnectionMode {
        provider
            .effective_models()
            .unwrap()
            .into_iter()
            .next()
            .expect("provider model")
            .transport
            .default_connection_mode
    }
}
