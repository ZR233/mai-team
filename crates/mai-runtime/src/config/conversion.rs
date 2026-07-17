use std::collections::BTreeMap;

use mai_protocol::{
    AgentConfigRequest, AgentModelPreference, ModelCapabilities as ApiModelCapabilities,
    ModelConfig as ApiModelConfig, ModelMaxTokensField, ModelReasoningConfig,
    ModelReasoningVariant, ModelRequestPolicy, ProviderConfig as ApiProviderConfig,
    ProviderConnectionMode as ApiProviderConnectionMode, ProviderConnectionModeDescriptor,
    ProviderModelCatalogConfig as ApiProviderModelCatalogConfig, ProviderSecret, ProviderSummary,
    ProviderTransportConfig as ApiProviderTransportConfig, ProviderTransportSummary,
    ProviderWireProtocol as ApiProviderWireProtocol, ProvidersConfigRequest, ProvidersResponse,
};
use pl_core::{
    AgentModelConfig, AgentRoleId, ModelCatalogId, ModelRouteConfig, ProviderConfig, ProviderId,
    ProviderModelCatalogConfig, ProviderPresetId, ReasoningEffort, builtin_provider_catalog,
};
use pl_model::{
    MaxTokensField, ModelInfo, ProviderConnectionMode, ProviderInfo, ProviderWireProtocol,
    ResponsesMaxTokensField,
};

use crate::model_profile::model_info;
use crate::{Result, RuntimeError};

const ROLES: [&str; 4] = ["planner", "explorer", "executor", "reviewer"];

/// 将轻量 HTTP DTO 单点转换为 PL 模型配置值对象。
///
/// `mai-protocol` 不依赖 `pl-core`；所有类型映射、默认角色选择和引用校验都集中在这里。
pub fn model_config_from_api(
    providers_request: &ProvidersConfigRequest,
    agents_request: &AgentConfigRequest,
) -> Result<AgentModelConfig> {
    let mut providers = BTreeMap::new();
    for provider in &providers_request.providers {
        let id = ProviderId::new(provider.id.clone()).map_err(RuntimeError::Model)?;
        if providers
            .insert(id.clone(), provider_from_api(provider)?)
            .is_some()
        {
            return Err(RuntimeError::InvalidInput(format!(
                "duplicate provider id: {id}"
            )));
        }
    }
    let fallback_provider = providers_request
        .default_provider_id
        .as_deref()
        .and_then(|id| {
            providers_request
                .providers
                .iter()
                .find(|provider| provider.id == id)
        })
        .or_else(|| {
            providers_request
                .providers
                .iter()
                .find(|provider| provider.enabled)
        })
        .or_else(|| providers_request.providers.first())
        .ok_or_else(|| {
            RuntimeError::InvalidInput("at least one provider is required".to_string())
        })?;
    let preferences = [
        agents_request.planner.as_ref(),
        agents_request.explorer.as_ref(),
        agents_request.executor.as_ref(),
        agents_request.reviewer.as_ref(),
    ];
    let routes = ROLES
        .into_iter()
        .zip(preferences)
        .map(|(role, preference)| {
            let preference = preference
                .cloned()
                .unwrap_or_else(|| fallback_preference(fallback_provider));
            Ok((
                AgentRoleId::new(role).map_err(RuntimeError::Model)?,
                route_from_preference(preference)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let models = AgentModelConfig { providers, routes };
    models.validate().map_err(RuntimeError::Model)?;
    Ok(models)
}

/// 将产品无关的 PL 模型配置投影回 mai 外部 provider DTO。
pub fn providers_request_from_models(models: &AgentModelConfig) -> ProvidersConfigRequest {
    let agent_config = agent_config_from_models(models);
    let providers = models
        .providers
        .iter()
        .map(|(id, provider)| {
            let protocol = provider
                .protocol()
                .expect("validated MaiConfig has a resolvable provider protocol");
            let effective_models = provider
                .effective_models()
                .expect("validated MaiConfig has a resolvable model catalog");
            let default_model = default_model_for_provider(models, id)
                .or_else(|| effective_models.first().map(|model| model.slug.clone()))
                .unwrap_or_default();
            ApiProviderConfig {
                id: id.to_string(),
                preset_id: provider.preset_id().map(ToString::to_string),
                transport: ApiProviderTransportConfig {
                    protocol: api_protocol(protocol),
                    connection_mode: api_connection_mode(provider.connection_mode()),
                },
                name: provider.name.clone(),
                base_url: provider.base_url.clone(),
                api_key: provider.bearer_token.clone(),
                api_key_env: provider.bearer_token_env.clone(),
                http_headers: provider.http_headers.as_ref().map(|headers| {
                    headers
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone()))
                        .collect()
                }),
                catalog: api_catalog(&provider.catalog, protocol),
                default_model,
                enabled: true,
            }
        })
        .collect();
    ProvidersConfigRequest {
        default_provider_id: agent_config
            .executor
            .map(|preference| preference.provider_id),
        providers,
    }
}

/// 构造不暴露 secret 的 provider HTTP response。
pub fn providers_response_from_models(models: &AgentModelConfig) -> ProvidersResponse {
    let request = providers_request_from_models(models);
    let configured_secrets = models
        .providers
        .iter()
        .map(|(id, provider)| (id.to_string(), provider.resolved_bearer_token().is_some()))
        .collect::<BTreeMap<_, _>>();
    let configured_modes = models
        .providers
        .iter()
        .map(|(id, provider)| {
            (
                id.to_string(),
                provider_connection_modes_for_instance(provider),
            )
        })
        .collect::<BTreeMap<_, _>>();
    ProvidersResponse {
        providers: request
            .providers
            .into_iter()
            .map(|provider| {
                let protocol = provider.transport.protocol;
                let has_api_key = configured_secrets
                    .get(&provider.id)
                    .copied()
                    .unwrap_or(false);
                let connection_modes = configured_modes
                    .get(&provider.id)
                    .cloned()
                    .unwrap_or_default();
                ProviderSummary {
                    id: provider.id,
                    preset_id: provider.preset_id,
                    transport: ProviderTransportSummary {
                        protocol: provider.transport.protocol,
                        connection_mode: provider.transport.connection_mode,
                        connection_modes,
                    },
                    name: provider.name,
                    base_url: provider.base_url,
                    api_key_env: provider.api_key_env,
                    models: effective_api_models(&provider.catalog, protocol),
                    catalog: provider.catalog,
                    default_model: provider.default_model,
                    enabled: provider.enabled,
                    has_api_key,
                }
            })
            .collect(),
        default_provider_id: request.default_provider_id,
    }
}

fn provider_connection_modes_for_instance(
    provider: &ProviderConfig,
) -> Vec<ProviderConnectionModeDescriptor> {
    let protocol = provider
        .protocol()
        .expect("validated MaiConfig has a resolvable provider protocol");
    let modes = provider
        .preset_id()
        .and_then(|preset_id| {
            builtin_provider_catalog()
                .presets
                .into_iter()
                .find(|preset| &preset.id == preset_id)
                .map(|preset| preset.connection_policy.supported_modes)
        })
        .unwrap_or_else(|| pl_core::provider_connection_modes(protocol));
    modes
        .into_iter()
        .map(|mode| ProviderConnectionModeDescriptor {
            id: match mode {
                ProviderConnectionMode::WebSocket => "web_socket",
                ProviderConnectionMode::Http => "http",
            }
            .to_string(),
            display_name: match mode {
                ProviderConnectionMode::WebSocket => "WebSocket",
                ProviderConnectionMode::Http => "HTTP",
            }
            .to_string(),
        })
        .collect()
}

/// 从动态角色路由生成 mai 角色配置 DTO。
pub fn agent_config_from_models(models: &AgentModelConfig) -> AgentConfigRequest {
    AgentConfigRequest {
        planner: route_preference(models, "planner"),
        explorer: route_preference(models, "explorer"),
        executor: route_preference(models, "executor"),
        reviewer: route_preference(models, "reviewer"),
    }
}

/// 从 MaiConfig 中解析 provider/model，并只在 DTO 边界构造旧选择值对象。
pub fn provider_selection_from_models(
    models: &AgentModelConfig,
    provider_id: Option<&str>,
    model: Option<&str>,
) -> Result<crate::ProviderSelection> {
    let fallback = models
        .resolve(&AgentRoleId::new("executor").map_err(RuntimeError::Model)?)
        .map_err(RuntimeError::Model)?;
    let provider_id = provider_id
        .filter(|value| !value.trim().is_empty())
        .map(ProviderId::new)
        .transpose()
        .map_err(RuntimeError::Model)?
        .unwrap_or(fallback.provider_id);
    let provider = models
        .providers
        .get(&provider_id)
        .ok_or_else(|| RuntimeError::InvalidInput(format!("provider `{provider_id}` not found")))?;
    let resolved_api_key = provider.resolved_bearer_token();
    if resolved_api_key.is_none() {
        return Err(RuntimeError::InvalidInput(format!(
            "provider `{provider_id}` has no API key"
        )));
    }
    let model_id = model
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| default_model_for_provider(models, &provider_id))
        .or_else(|| {
            provider
                .effective_models()
                .ok()
                .and_then(|models| models.first().map(|model| model.slug.clone()))
        })
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!("provider `{provider_id}` has no model"))
        })?;
    let effective_models = provider.effective_models().map_err(RuntimeError::Model)?;
    let model = effective_models
        .iter()
        .find(|candidate| candidate.slug == model_id)
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "model `{model_id}` is not configured for provider `{provider_id}`"
            ))
        })?;
    let protocol = provider.protocol().map_err(RuntimeError::Model)?;
    let selected_model = api_model(model, protocol);
    if selected_model.context_tokens == 0 || selected_model.output_tokens == 0 {
        return Err(RuntimeError::InvalidInput(format!(
            "model `{model_id}` must configure context_tokens and output_tokens"
        )));
    }
    Ok(crate::ProviderSelection {
        provider: ProviderSecret {
            id: provider_id.to_string(),
            transport: ApiProviderTransportConfig {
                protocol: api_protocol(protocol),
                connection_mode: api_connection_mode(provider.connection_mode()),
            },
            name: provider.name.clone(),
            base_url: provider.base_url.clone(),
            api_key: resolved_api_key.unwrap_or_default(),
            api_key_env: provider.bearer_token_env.clone(),
            http_headers: provider
                .http_headers
                .as_ref()
                .map(|headers| {
                    headers
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            models: effective_models
                .iter()
                .map(|model| api_model(model, protocol))
                .collect(),
            default_model: model_id,
            enabled: true,
        },
        model: selected_model,
    })
}

pub(super) fn provider_from_api(provider: &ApiProviderConfig) -> Result<ProviderConfig> {
    let protocol = core_protocol(provider.transport.protocol);
    let preset_id = provider
        .preset_id
        .clone()
        .map(ProviderPresetId::new)
        .transpose()
        .map_err(RuntimeError::Model)?;
    let preset = preset_id.as_ref().and_then(|id| {
        builtin_provider_catalog()
            .presets
            .into_iter()
            .find(|preset| &preset.id == id)
    });
    if let Some(preset) = &preset
        && preset.protocol != protocol
    {
        return Err(RuntimeError::InvalidInput(format!(
            "provider preset `{}` requires {:?}, got {:?}",
            preset.id, preset.protocol, protocol
        )));
    }
    let mut info = match preset {
        Some(preset) => preset
            .provider
            .to_provider_info(&provider.default_model)
            .map_err(RuntimeError::Model)?,
        None => match protocol {
            ProviderWireProtocol::Responses => ProviderInfo::responses_compatible(
                provider.name.clone(),
                provider.base_url.clone(),
                provider.default_model.clone(),
            ),
            ProviderWireProtocol::ChatCompletions => ProviderInfo::openai_compatible_chat(
                provider.name.clone(),
                provider.base_url.clone(),
                provider.default_model.clone(),
            ),
        },
    };
    info.name = provider.name.clone();
    info.base_url = provider.base_url.clone();
    info.default_model = provider.default_model.clone();
    info.protocol = protocol;
    info.connection_mode = core_connection_mode(provider.transport.connection_mode);
    info.bearer_token = provider.api_key.clone();
    if let Some(headers) = &provider.http_headers {
        info.http_headers = Some(
            headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        );
    }
    let catalog = match &provider.catalog {
        ApiProviderModelCatalogConfig::Bundled {
            catalog_id,
            additional_models,
        } => ProviderModelCatalogConfig::Bundled {
            catalog: ModelCatalogId::new(catalog_id.clone()).map_err(RuntimeError::Model)?,
            additional_models: additional_models
                .iter()
                .map(|model| model_info(model, provider.transport.protocol))
                .collect(),
        },
        ApiProviderModelCatalogConfig::Explicit { models } => {
            ProviderModelCatalogConfig::Explicit {
                models: models
                    .iter()
                    .map(|model| model_info(model, provider.transport.protocol))
                    .collect(),
            }
        }
    };
    let mut config = match catalog {
        ProviderModelCatalogConfig::Bundled {
            catalog,
            additional_models,
        } => ProviderConfig::from_bundled_catalog(info, catalog, additional_models),
        ProviderModelCatalogConfig::Explicit { models } => {
            ProviderConfig::from_explicit_models(info, models)
        }
    };
    config.bearer_token_env = provider.api_key_env.clone();
    if let Some(preset_id) = preset_id {
        config = config.with_preset(preset_id);
    }
    config.effective_models().map_err(RuntimeError::Model)?;
    Ok(config)
}

/// 保留 Web DTO 不拥有的 provider 运行时字段，同时避免把旧 header 带到新 endpoint。
pub(crate) fn preserve_provider_private_fields(
    current: &AgentModelConfig,
    request: &ProvidersConfigRequest,
    next: &mut AgentModelConfig,
) {
    for requested in &request.providers {
        let Ok(id) = ProviderId::new(requested.id.clone()) else {
            continue;
        };
        let (Some(current_provider), Some(next_provider)) =
            (current.providers.get(&id), next.providers.get_mut(&id))
        else {
            continue;
        };
        if !same_private_config_scope(current_provider, requested) {
            continue;
        }
        if requested.http_headers.is_none() {
            next_provider.http_headers = current_provider.http_headers.clone();
        }
        next_provider.tool_wire_policy = current_provider.tool_wire_policy;
        next_provider.apply_patch_tool_type = current_provider.apply_patch_tool_type;
    }
}

fn same_private_config_scope(current: &ProviderConfig, requested: &ApiProviderConfig) -> bool {
    current.preset_id().map(ToString::to_string) == requested.preset_id
        && current.protocol().ok().map(api_protocol) == Some(requested.transport.protocol)
        && normalized_url(&current.base_url) == normalized_url(&requested.base_url)
}

fn normalized_url(value: &str) -> &str {
    value.trim().trim_end_matches('/')
}

fn route_preference(models: &AgentModelConfig, role: &str) -> Option<AgentModelPreference> {
    let route = models.routes.get(&AgentRoleId::new(role).ok()?)?;
    Some(AgentModelPreference {
        provider_id: route.provider.to_string(),
        model: route.model.clone(),
        reasoning_effort: route
            .reasoning_effort
            .as_ref()
            .map(|effort| effort.as_str().to_string()),
    })
}

fn default_model_for_provider(models: &AgentModelConfig, provider: &ProviderId) -> Option<String> {
    ["executor", "planner", "explorer", "reviewer"]
        .into_iter()
        .filter_map(|role| AgentRoleId::new(role).ok())
        .filter_map(|role| models.routes.get(&role))
        .find(|route| &route.provider == provider)
        .map(|route| route.model.clone())
}

fn api_protocol(protocol: ProviderWireProtocol) -> ApiProviderWireProtocol {
    match protocol {
        ProviderWireProtocol::Responses => ApiProviderWireProtocol::Responses,
        ProviderWireProtocol::ChatCompletions => ApiProviderWireProtocol::ChatCompletions,
    }
}

fn api_connection_mode(mode: ProviderConnectionMode) -> ApiProviderConnectionMode {
    match mode {
        ProviderConnectionMode::WebSocket => ApiProviderConnectionMode::WebSocket,
        ProviderConnectionMode::Http => ApiProviderConnectionMode::Http,
    }
}

fn core_connection_mode(mode: ApiProviderConnectionMode) -> ProviderConnectionMode {
    match mode {
        ApiProviderConnectionMode::WebSocket => ProviderConnectionMode::WebSocket,
        ApiProviderConnectionMode::Http => ProviderConnectionMode::Http,
    }
}

fn api_model(model: &ModelInfo, protocol: ProviderWireProtocol) -> ApiModelConfig {
    let reasoning = model
        .effort_parameter()
        .map(|parameter| ModelReasoningConfig {
            default_variant: parameter.candidates.first().cloned(),
            variants: parameter
                .candidates
                .iter()
                .map(|candidate| ModelReasoningVariant {
                    id: candidate.clone(),
                    label: None,
                    request: reasoning_request(model, candidate),
                })
                .collect(),
        });
    ApiModelConfig {
        id: model.slug.clone(),
        name: Some(model.display_name.clone()),
        context_tokens: model.resolved_context_window().unwrap_or(128_000),
        max_context_tokens: model.max_context_window,
        effective_context_window_percent: 95,
        output_tokens: model.max_output_tokens.unwrap_or(4096),
        auto_compact_token_limit: model.auto_compact_token_limit,
        supports_tools: model.capabilities.tools.function_calling,
        capabilities: ApiModelCapabilities {
            tools: model.capabilities.tools.function_calling,
            parallel_tools: model.capabilities.tools.parallel_tool_calls,
            reasoning_replay: model.capabilities.reasoning,
            strict_schema: false,
        },
        request_policy: ModelRequestPolicy {
            max_tokens_field: match protocol {
                ProviderWireProtocol::Responses => {
                    match model.request_profile.responses_max_tokens_field {
                        ResponsesMaxTokensField::Omit => ModelMaxTokensField::Omit,
                        ResponsesMaxTokensField::MaxOutputTokens => {
                            ModelMaxTokensField::MaxOutputTokens
                        }
                        ResponsesMaxTokensField::MaxCompletionTokens => {
                            ModelMaxTokensField::MaxCompletionTokens
                        }
                        ResponsesMaxTokensField::MaxTokens => ModelMaxTokensField::MaxTokens,
                    }
                }
                ProviderWireProtocol::ChatCompletions => {
                    match model.request_profile.max_tokens_field {
                        MaxTokensField::MaxTokens => ModelMaxTokensField::MaxTokens,
                        MaxTokensField::MaxCompletionTokens => {
                            ModelMaxTokensField::MaxCompletionTokens
                        }
                    }
                }
            },
            store: None,
            headers: model.request_profile.headers.clone().into_iter().collect(),
            extra_body: serde_json::Value::Object(model.request_profile.body.clone()),
            ..ModelRequestPolicy::default()
        },
        reasoning,
        options: serde_json::Value::Object(model.request_profile.options.clone()),
        headers: BTreeMap::new(),
    }
}

fn reasoning_request(model: &ModelInfo, effort: &str) -> serde_json::Value {
    let mut body = serde_json::Map::new();
    if let Some(wire) = model
        .effort_parameter()
        .and_then(|parameter| parameter.wire_for(effort))
    {
        wire.apply_to(&mut body);
    }
    serde_json::Value::Object(body)
}

fn api_catalog(
    catalog: &ProviderModelCatalogConfig,
    protocol: ProviderWireProtocol,
) -> ApiProviderModelCatalogConfig {
    match catalog {
        ProviderModelCatalogConfig::Bundled {
            catalog,
            additional_models,
        } => ApiProviderModelCatalogConfig::Bundled {
            catalog_id: catalog.to_string(),
            additional_models: additional_models
                .iter()
                .map(|model| api_model(model, protocol))
                .collect(),
        },
        ProviderModelCatalogConfig::Explicit { models } => {
            ApiProviderModelCatalogConfig::Explicit {
                models: models
                    .iter()
                    .map(|model| api_model(model, protocol))
                    .collect(),
            }
        }
    }
}

fn effective_api_models(
    catalog: &ApiProviderModelCatalogConfig,
    protocol: ApiProviderWireProtocol,
) -> Vec<ApiModelConfig> {
    let protocol = core_protocol(protocol);
    match catalog {
        ApiProviderModelCatalogConfig::Bundled {
            catalog_id,
            additional_models,
        } => {
            let id = ModelCatalogId::new(catalog_id.clone())
                .expect("validated catalog id can be projected");
            let mut models = pl_core::builtin_model_catalog(&id)
                .expect("validated bundled catalog can be projected")
                .models
                .iter()
                .map(|model| api_model(model, protocol))
                .collect::<Vec<_>>();
            models.extend(additional_models.clone());
            models
        }
        ApiProviderModelCatalogConfig::Explicit { models } => models.clone(),
    }
}

fn core_protocol(protocol: ApiProviderWireProtocol) -> ProviderWireProtocol {
    match protocol {
        ApiProviderWireProtocol::Responses => ProviderWireProtocol::Responses,
        ApiProviderWireProtocol::ChatCompletions => ProviderWireProtocol::ChatCompletions,
    }
}

fn fallback_preference(provider: &ApiProviderConfig) -> AgentModelPreference {
    let effective_models = effective_api_models(&provider.catalog, provider.transport.protocol);
    let reasoning_effort = effective_models
        .iter()
        .find(|model| model.id == provider.default_model)
        .and_then(|model| model.reasoning.as_ref())
        .and_then(|reasoning| reasoning.default_variant.clone());
    AgentModelPreference {
        provider_id: provider.id.clone(),
        model: provider.default_model.clone(),
        reasoning_effort,
    }
}

fn route_from_preference(preference: AgentModelPreference) -> Result<ModelRouteConfig> {
    if preference.model.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "agent model preference has empty model".to_string(),
        ));
    }
    Ok(ModelRouteConfig {
        provider: ProviderId::new(preference.provider_id).map_err(RuntimeError::Model)?,
        model: preference.model,
        reasoning_effort: preference.reasoning_effort.map(ReasoningEffort::new),
    })
}

#[cfg(test)]
mod tests {
    use mai_protocol::{ModelConfig, ProviderTransportConfig, ProviderWireProtocol};
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn api_conversion_creates_dynamic_routes_without_provider_default_model() {
        let providers = ProvidersConfigRequest {
            providers: vec![ApiProviderConfig {
                id: "custom".to_string(),
                preset_id: None,
                transport: ProviderTransportConfig {
                    protocol: ProviderWireProtocol::Responses,
                    connection_mode: ApiProviderConnectionMode::WebSocket,
                },
                name: "Custom".to_string(),
                base_url: "https://example.test/v1".to_string(),
                api_key: Some("secret".to_string()),
                api_key_env: None,
                http_headers: Some(BTreeMap::from([(
                    "x-openai-actor-authorization".to_string(),
                    "local-image-extension".to_string(),
                )])),
                catalog: ApiProviderModelCatalogConfig::Explicit {
                    models: vec![ModelConfig {
                        id: "model-a".to_string(),
                        name: None,
                        context_tokens: 128_000,
                        max_context_tokens: None,
                        effective_context_window_percent: 95,
                        output_tokens: 4096,
                        auto_compact_token_limit: None,
                        supports_tools: true,
                        capabilities: Default::default(),
                        request_policy: Default::default(),
                        reasoning: None,
                        options: serde_json::Value::Null,
                        headers: BTreeMap::new(),
                    }],
                },
                default_model: "model-a".to_string(),
                enabled: true,
            }],
            default_provider_id: Some("custom".to_string()),
        };

        let config = model_config_from_api(&providers, &AgentConfigRequest::default()).unwrap();
        let route = config
            .resolve(&AgentRoleId::new("executor").unwrap())
            .unwrap();

        assert_eq!(route.provider_id, ProviderId::new("custom").unwrap());
        assert_eq!(route.model.slug, "model-a");
        assert_eq!(route.provider_info.default_model, "model-a");
        assert_eq!(
            route.provider_info.http_headers,
            Some(std::collections::HashMap::from([(
                "x-openai-actor-authorization".to_string(),
                "local-image-extension".to_string(),
            )]))
        );

        let selection =
            provider_selection_from_models(&config, Some("custom"), Some("model-a")).unwrap();
        assert_eq!(
            selection.provider.http_headers,
            BTreeMap::from([(
                "x-openai-actor-authorization".to_string(),
                "local-image-extension".to_string(),
            )])
        );
        assert_eq!(
            crate::model_profile::provider_info(&selection.provider).http_headers,
            route.provider_info.http_headers
        );

        let agent_projection = agent_config_from_models(&config);
        for preference in [
            agent_projection.planner,
            agent_projection.explorer,
            agent_projection.executor,
            agent_projection.reviewer,
        ] {
            let preference = preference.expect("required role projection");
            assert_eq!(preference.provider_id, "custom");
            assert_eq!(preference.model, "model-a");
        }
        let provider_projection = providers_request_from_models(&config);
        assert_eq!(
            provider_projection.providers[0].api_key.as_deref(),
            Some("secret")
        );
        let public_projection = providers_response_from_models(&config);
        assert!(public_projection.providers[0].has_api_key);
    }

    #[test]
    fn omitted_headers_preserve_unchanged_provider_private_fields() {
        let mut current = crate::MaiConfig::default().models;
        let provider_id = ProviderId::new("deepseek").unwrap();
        let current_provider = current.providers.get_mut(&provider_id).unwrap();
        current_provider.http_headers = Some(std::collections::HashMap::from([(
            "x-provider-scope".to_string(),
            "configured".to_string(),
        )]));
        current_provider.tool_wire_policy = pl_model::ToolWirePolicy::NativeCustomTools;
        current_provider.apply_patch_tool_type = Some(pl_model::ApplyPatchToolType::Freeform);
        let expected_headers = current_provider.http_headers.clone();
        let expected_tool_wire_policy = current_provider.tool_wire_policy;
        let expected_apply_patch_tool_type = current_provider.apply_patch_tool_type;
        let mut request = providers_request_from_models(&current);
        request.providers[0].http_headers = None;
        let roles = agent_config_from_models(&current);
        let mut next = model_config_from_api(&request, &roles).unwrap();

        preserve_provider_private_fields(&current, &request, &mut next);

        let next_provider = next.providers.get(&provider_id).unwrap();
        assert_eq!(next_provider.http_headers, expected_headers);
        assert_eq!(next_provider.tool_wire_policy, expected_tool_wire_policy);
        assert_eq!(
            next_provider.apply_patch_tool_type,
            expected_apply_patch_tool_type
        );
    }

    #[test]
    fn responses_omit_max_tokens_policy_survives_product_projection() {
        let catalog = pl_core::builtin_model_catalog(&ModelCatalogId::new("openai").unwrap())
            .expect("built-in OpenAI catalog");
        let model = catalog
            .models
            .iter()
            .find(|model| model.slug == "gpt-5.5")
            .expect("built-in gpt-5.5");

        let projected = api_model(model, pl_model::ProviderWireProtocol::Responses);
        let restored = model_info(&projected, ApiProviderWireProtocol::Responses);

        assert_eq!(
            projected.request_policy.max_tokens_field,
            ModelMaxTokensField::Omit
        );
        assert_eq!(
            restored.request_profile.responses_max_tokens_field,
            ResponsesMaxTokensField::Omit
        );
    }

    #[test]
    fn chat_max_tokens_policy_is_independent_from_responses_policy() {
        let catalog = pl_core::builtin_model_catalog(&ModelCatalogId::new("deepseek").unwrap())
            .expect("built-in DeepSeek catalog");
        let model = catalog.models.first().expect("built-in chat model");

        let projected = api_model(model, pl_model::ProviderWireProtocol::ChatCompletions);
        let restored = model_info(&projected, ApiProviderWireProtocol::ChatCompletions);

        assert_eq!(
            projected.request_policy.max_tokens_field,
            ModelMaxTokensField::MaxTokens
        );
        assert_eq!(
            restored.request_profile.max_tokens_field,
            MaxTokensField::MaxTokens
        );
        assert_eq!(
            restored.request_profile.responses_max_tokens_field,
            ResponsesMaxTokensField::Omit
        );
    }

    #[test]
    fn same_preset_supports_independent_ws_and_http_instances() {
        let provider = |id: &str, connection_mode| ApiProviderConfig {
            id: id.to_string(),
            preset_id: Some("openai".to_string()),
            transport: ProviderTransportConfig {
                protocol: ProviderWireProtocol::Responses,
                connection_mode,
            },
            name: format!("OpenAI {id}"),
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: Some(format!("secret-{id}")),
            api_key_env: None,
            http_headers: None,
            catalog: ApiProviderModelCatalogConfig::Bundled {
                catalog_id: "openai".to_string(),
                additional_models: Vec::new(),
            },
            default_model: "gpt-5.6-sol".to_string(),
            enabled: true,
        };
        let request = ProvidersConfigRequest {
            providers: vec![
                provider("openai-ws", ApiProviderConnectionMode::WebSocket),
                provider("openai-http", ApiProviderConnectionMode::Http),
            ],
            default_provider_id: Some("openai-ws".to_string()),
        };

        let models = model_config_from_api(&request, &AgentConfigRequest::default()).unwrap();
        let response = providers_response_from_models(&models);

        assert_eq!(response.providers.len(), 2);
        assert_eq!(
            response.providers[0]
                .transport
                .connection_modes
                .iter()
                .map(|mode| mode.id.as_str())
                .collect::<Vec<_>>(),
            vec!["web_socket", "http"]
        );
        assert_eq!(
            response
                .providers
                .iter()
                .find(|provider| provider.id == "openai-ws")
                .unwrap()
                .transport
                .connection_mode,
            ApiProviderConnectionMode::WebSocket
        );
        assert_eq!(
            response
                .providers
                .iter()
                .find(|provider| provider.id == "openai-http")
                .unwrap()
                .transport
                .connection_mode,
            ApiProviderConnectionMode::Http
        );
    }

    #[test]
    fn duplicate_provider_ids_are_rejected_instead_of_overwriting() {
        let mut provider = providers_request_from_models(&crate::MaiConfig::default().models)
            .providers
            .into_iter()
            .next()
            .expect("default provider");
        provider.id = "duplicate".to_string();
        let request = ProvidersConfigRequest {
            providers: vec![provider.clone(), provider],
            default_provider_id: Some("duplicate".to_string()),
        };

        let error = model_config_from_api(&request, &AgentConfigRequest::default()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("duplicate provider id: duplicate")
        );
    }
}
