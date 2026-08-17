use std::collections::BTreeMap;

use mai_protocol::{
    AgentConfigRequest, AgentRole, ProviderConfig as ApiProviderConfig, ProviderConfigSource,
    ProviderSummary, ProvidersConfigRequest, ProvidersResponse,
};
use pl_core::{
    AgentModelConfig, AgentRoleId, ModelRouteConfig, ProviderConfig, ProviderId, ReasoningEffort,
    ResolvedModelRoute, builtin_provider_catalog,
};

use crate::{Result, RuntimeError};

const ROLES: [(AgentRole, &str); 4] = [
    (AgentRole::Planner, "planner"),
    (AgentRole::Explorer, "explorer"),
    (AgentRole::Executor, "executor"),
    (AgentRole::Reviewer, "reviewer"),
];

/// 将 mai 的 provider 实例包装与角色配置组合为 PL 的 canonical 模型配置。
///
/// Preset 实例始终从 PL registry 重新实例化，只接受名称、endpoint 与凭证等
/// 实例字段；custom provider 则完整保留调用方提交的 PL `ProviderConfig`。
pub fn model_config_from_api(
    providers_request: &ProvidersConfigRequest,
    agents_request: &AgentConfigRequest,
) -> Result<AgentModelConfig> {
    let mut providers = BTreeMap::new();
    for provider in &providers_request.providers {
        let id = ProviderId::new(provider.id.clone()).map_err(RuntimeError::Model)?;
        if providers
            .insert(id.clone(), canonical_provider(provider)?)
            .is_some()
        {
            return Err(RuntimeError::InvalidInput(format!(
                "duplicate provider id: {id}"
            )));
        }
    }
    let routes = ROLES
        .into_iter()
        .map(|(role, id)| {
            Ok((
                AgentRoleId::new(id).map_err(RuntimeError::Model)?,
                required_route(agents_request, role)?.clone(),
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let models = AgentModelConfig { providers, routes };
    models.validate().map_err(RuntimeError::Model)?;
    Ok(models)
}

/// 返回完整的内部 provider 请求值；仅用于服务端原子更新，不能直接作为 HTTP 响应。
pub fn providers_request_from_models(models: &AgentModelConfig) -> ProvidersConfigRequest {
    ProvidersConfigRequest {
        providers: models
            .providers
            .iter()
            .map(|(id, config)| ApiProviderConfig {
                id: id.to_string(),
                source: api_source_from_provider(config),
            })
            .collect(),
    }
}

/// 构造不暴露 bearer token 或私有 headers 的 provider HTTP 响应。
pub fn providers_response_from_models(models: &AgentModelConfig) -> ProvidersResponse {
    ProvidersResponse {
        providers: models
            .providers
            .iter()
            .map(|(id, provider)| {
                let models = provider
                    .effective_models()
                    .expect("validated MaiConfig has a resolvable model catalog");
                let has_api_key = provider.resolved_bearer_token().is_some();
                let has_http_headers = provider
                    .http_headers
                    .as_ref()
                    .is_some_and(|headers| !headers.is_empty());
                let mut config = provider.clone();
                config.bearer_token = None;
                config.http_headers = None;
                ProviderSummary {
                    id: id.to_string(),
                    config,
                    models,
                    has_api_key,
                    has_http_headers,
                }
            })
            .collect(),
    }
}

/// 从 PL 配置直接投影四个 mai 产品角色。
pub fn agent_config_from_models(models: &AgentModelConfig) -> AgentConfigRequest {
    AgentConfigRequest {
        planner: route(models, "planner"),
        explorer: route(models, "explorer"),
        executor: route(models, "executor"),
        reviewer: route(models, "reviewer"),
    }
}

/// 解析 provider smoke 等非角色调用使用的 provider/model。
///
/// 该函数只建立一次临时 PL route，并仍由 `AgentModelConfig::resolve` 完成全部
/// provider、model 与 reasoning 校验，不在 mai 重建任何模型语义。
pub fn resolve_provider_model(
    models: &AgentModelConfig,
    provider_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<ResolvedModelRoute> {
    let executor = models
        .routes
        .get(&AgentRoleId::new("executor").map_err(RuntimeError::Model)?);
    let provider_id = provider_id
        .filter(|value| !value.trim().is_empty())
        .map(ProviderId::new)
        .transpose()
        .map_err(RuntimeError::Model)?
        .or_else(|| executor.map(|route| route.provider.clone()))
        .or_else(|| models.providers.keys().next().cloned())
        .ok_or_else(|| RuntimeError::InvalidInput("at least one provider is required".into()))?;
    let provider = models
        .providers
        .get(&provider_id)
        .ok_or_else(|| RuntimeError::InvalidInput(format!("provider `{provider_id}` not found")))?;
    let declared_models = provider.effective_models().map_err(RuntimeError::Model)?;
    let model = model
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            executor
                .filter(|route| route.provider == provider_id)
                .map(|route| route.model.clone())
        })
        .or_else(|| declared_models.first().map(|model| model.slug.clone()))
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!("provider `{provider_id}` has no model"))
        })?;
    let selected = declared_models
        .iter()
        .find(|candidate| candidate.slug == model)
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "model `{model}` is not configured for provider `{provider_id}`"
            ))
        })?;
    let effort = effort
        .map(ReasoningEffort::new)
        .or_else(|| selected.default_effort().map(ReasoningEffort::new));
    let role = AgentRoleId::new("provider-test").map_err(RuntimeError::Model)?;
    let mut scoped = models.clone();
    scoped.routes.insert(
        role.clone(),
        ModelRouteConfig {
            provider: provider_id,
            model,
            effort,
        },
    );
    let route = scoped.resolve(&role).map_err(RuntimeError::Model)?;
    if route.provider_info.bearer_token.is_none() {
        return Err(RuntimeError::InvalidInput(format!(
            "provider `{}` has no API key",
            route.provider_id
        )));
    }
    Ok(route)
}

/// 在同一 provider 身份和 endpoint 下实现 write-only secret 的 keep/set/clear。
///
/// `None` 表示 keep，非空值表示 set，空 token/空 headers 表示 clear。身份或
/// endpoint 改变时不会把旧 secret 带到新作用域。
pub fn preserve_provider_secrets(current: &AgentModelConfig, request: &mut ProvidersConfigRequest) {
    for requested in &mut request.providers {
        let Ok(id) = ProviderId::new(requested.id.clone()) else {
            continue;
        };
        let Some(existing) = current.providers.get(&id) else {
            continue;
        };
        if !same_private_scope(existing, &requested.source) {
            continue;
        }
        let (bearer_token, http_headers) = private_fields_mut(&mut requested.source);
        if bearer_token.is_none() {
            *bearer_token = existing.bearer_token.clone();
        } else if bearer_token
            .as_deref()
            .is_some_and(|token| token.trim().is_empty())
        {
            *bearer_token = None;
        }
        if http_headers.is_none() {
            *http_headers = existing.http_headers.clone();
        }
    }
}

fn canonical_provider(provider: &ApiProviderConfig) -> Result<ProviderConfig> {
    match &provider.source {
        ProviderConfigSource::Preset {
            preset_id,
            name,
            base_url,
            bearer_token,
            bearer_token_env,
            http_headers,
        } => {
            let preset = builtin_provider_catalog()
                .presets
                .into_iter()
                .find(|preset| preset.id.as_str() == preset_id)
                .ok_or_else(|| {
                    RuntimeError::InvalidInput(format!("unknown provider preset: {preset_id}"))
                })?;
            let mut canonical = preset.provider;
            canonical.name.clone_from(name);
            canonical.base_url.clone_from(base_url);
            canonical.bearer_token.clone_from(bearer_token);
            canonical.bearer_token_env.clone_from(bearer_token_env);
            canonical.http_headers.clone_from(http_headers);
            Ok(canonical)
        }
        ProviderConfigSource::Custom { config } => {
            if let Some(preset_id) = config.preset_id() {
                return Err(RuntimeError::InvalidInput(format!(
                    "custom provider must not declare preset `{preset_id}`"
                )));
            }
            Ok(config.clone())
        }
    }
}

fn required_route(request: &AgentConfigRequest, role: AgentRole) -> Result<&ModelRouteConfig> {
    role_route(request, role)
        .ok_or_else(|| RuntimeError::InvalidInput(format!("missing `{role}` model route")))
}

fn role_route(request: &AgentConfigRequest, role: AgentRole) -> Option<&ModelRouteConfig> {
    match role {
        AgentRole::Planner => request.planner.as_ref(),
        AgentRole::Explorer => request.explorer.as_ref(),
        AgentRole::Executor => request.executor.as_ref(),
        AgentRole::Reviewer => request.reviewer.as_ref(),
    }
}

fn route(models: &AgentModelConfig, role: &str) -> Option<ModelRouteConfig> {
    AgentRoleId::new(role)
        .ok()
        .and_then(|role| models.routes.get(&role).cloned())
}

fn api_source_from_provider(provider: &ProviderConfig) -> ProviderConfigSource {
    if let Some(preset_id) = provider.preset_id() {
        ProviderConfigSource::Preset {
            preset_id: preset_id.to_string(),
            name: provider.name.clone(),
            base_url: provider.base_url.clone(),
            bearer_token: provider.bearer_token.clone(),
            bearer_token_env: provider.bearer_token_env.clone(),
            http_headers: provider.http_headers.clone(),
        }
    } else {
        ProviderConfigSource::Custom {
            config: provider.clone(),
        }
    }
}

fn same_private_scope(current: &ProviderConfig, requested: &ProviderConfigSource) -> bool {
    let (preset_id, base_url) = match requested {
        ProviderConfigSource::Preset {
            preset_id,
            base_url,
            ..
        } => (Some(preset_id.as_str()), base_url.as_str()),
        ProviderConfigSource::Custom { config } => (
            config.preset_id().map(|id| id.as_str()),
            config.base_url.as_str(),
        ),
    };
    current.preset_id().map(|id| id.as_str()) == preset_id
        && current.base_url.trim_end_matches('/') == base_url.trim_end_matches('/')
}

fn private_fields_mut(
    source: &mut ProviderConfigSource,
) -> (
    &mut Option<String>,
    &mut Option<std::collections::HashMap<String, String>>,
) {
    match source {
        ProviderConfigSource::Preset {
            bearer_token,
            http_headers,
            ..
        } => (bearer_token, http_headers),
        ProviderConfigSource::Custom { config } => {
            (&mut config.bearer_token, &mut config.http_headers)
        }
    }
}

#[cfg(test)]
mod tests {
    use pl_core::{ModelCatalogId, ProviderModelCatalogConfig};
    use pl_model::{ModelInfo, ProviderInfo};
    use pretty_assertions::assert_eq;

    use super::*;

    fn routes(provider: &str, model: &str, effort: Option<&str>) -> AgentConfigRequest {
        let route = ModelRouteConfig {
            provider: ProviderId::new(provider).unwrap(),
            model: model.to_string(),
            effort: effort.map(ReasoningEffort::new),
        };
        AgentConfigRequest {
            planner: Some(route.clone()),
            explorer: Some(route.clone()),
            executor: Some(route.clone()),
            reviewer: Some(route),
        }
    }

    #[test]
    fn preset_provider_uses_pl_catalog_and_discards_submitted_model_overrides() {
        let preset = builtin_provider_catalog()
            .presets
            .into_iter()
            .find(|preset| preset.id.as_str() == "openai")
            .unwrap();
        let request = ProvidersConfigRequest {
            providers: vec![ApiProviderConfig {
                id: "openai".to_string(),
                source: ProviderConfigSource::Preset {
                    preset_id: "openai".to_string(),
                    name: "OpenAI proxy".to_string(),
                    base_url: "https://proxy.example/v1".to_string(),
                    bearer_token: Some("secret".to_string()),
                    bearer_token_env: None,
                    http_headers: None,
                },
            }],
        };
        let models = model_config_from_api(
            &request,
            &routes("openai", &preset.suggested_model, Some("low")),
        )
        .unwrap();
        let provider = models
            .providers
            .get(&ProviderId::new("openai").unwrap())
            .unwrap();
        assert_eq!(provider.base_url, "https://proxy.example/v1");
        assert!(matches!(
            provider.catalog,
            ProviderModelCatalogConfig::Bundled { .. }
        ));
        assert!(provider.connection_overrides().is_empty());
    }

    #[test]
    fn custom_provider_round_trips_without_projection() {
        let mut model = ModelInfo::fallback("custom-model");
        model.used_fallback = false;
        let mut provider = ProviderConfig::from_provider_info(
            ProviderInfo::responses_compatible("Custom", "https://example.test/v1", "custom-model"),
            vec![model],
        );
        provider.bearer_token = Some("secret".to_string());
        let request = ProvidersConfigRequest {
            providers: vec![ApiProviderConfig {
                id: "custom".to_string(),
                source: ProviderConfigSource::Custom {
                    config: provider.clone(),
                },
            }],
        };
        let models =
            model_config_from_api(&request, &routes("custom", "custom-model", None)).unwrap();
        assert_eq!(
            models.providers.get(&ProviderId::new("custom").unwrap()),
            Some(&provider)
        );
    }

    #[test]
    fn public_projection_redacts_secrets_without_losing_model_values() {
        let mut provider = ProviderConfig::from_bundled_catalog(
            ProviderInfo::openai(None),
            ModelCatalogId::new("openai").unwrap(),
            Vec::new(),
        );
        provider.bearer_token = Some("secret".to_string());
        provider.http_headers = Some(std::collections::HashMap::from([(
            "x-private".into(),
            "value".into(),
        )]));
        let models = AgentModelConfig {
            providers: BTreeMap::from([(ProviderId::new("openai").unwrap(), provider)]),
            routes: BTreeMap::new(),
        };
        let response = providers_response_from_models(&models);
        let summary = &response.providers[0];
        assert!(summary.has_api_key);
        assert!(summary.has_http_headers);
        assert_eq!(summary.config.bearer_token, None);
        assert_eq!(summary.config.http_headers, None);
        assert!(!summary.models.is_empty());
    }
}
