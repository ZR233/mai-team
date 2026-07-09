use std::collections::{BTreeMap, HashMap};

use mai_protocol::{
    ModelCapabilities as MaiModelCapabilities, ModelConfig, ModelWireApi,
    ProviderKind as MaiProviderKind, ProviderSecret,
};
use mai_store::ProviderSelection;
use pl_core::{
    CoreModelContinuationConfig, CoreModelContinuationProfile, CoreModelProviderFamily,
    CoreModelTurnRequest, CoreModelWireApi,
};
use pl_model::{
    MaxTokensField, MissingCandidatePolicy, ModelCapabilities, ModelInfo, ModelModality,
    ModelParameter, ModelParameterCandidateRequest, ModelRequestProfile, ParameterWire,
    ProviderInfo, ReasoningConfig, ReasoningSummary, SharedModelProvider, ToolCapabilities,
    ToolSchema, create_provider_with_models,
};
use pl_protocol::PureError;

/// 将 mai 的 provider/model 选择投影成 pl-core 可直接执行的 provider。
pub fn core_provider_for_selection(
    selection: &ProviderSelection,
) -> Result<SharedModelProvider, PureError> {
    let mut info = provider_info(&selection.provider);
    info.default_model = selection.model.id.clone();
    create_provider_with_models(info, vec![model_info(&selection.model)])
}

/// 将 mai 的模型配置投影成 pl-core 的单次模型请求。
pub fn core_model_turn_request(
    selection: &ProviderSelection,
    reasoning_effort: Option<&str>,
    instructions: impl Into<String>,
    tools: Vec<ToolSchema>,
) -> CoreModelTurnRequest {
    CoreModelTurnRequest::new(selection.model.id.clone())
        .with_instructions(instructions)
        .with_tools(tools)
        .with_parallel_tool_calls(selection.model.capabilities.parallel_tools)
        .with_max_tokens(Some(selection.model.output_tokens))
        .with_reasoning(reasoning_config(&selection.model, reasoning_effort))
        .with_continuation_config(model_continuation_config(selection))
}

/// 判断当前 mai provider/model 选择是否能走 pl-core continuation 路径。
pub fn model_supports_continuation(selection: &ProviderSelection) -> bool {
    model_continuation_config(selection).enabled()
}

pub(crate) fn provider_info(provider: &ProviderSecret) -> ProviderInfo {
    let mut info = match provider.kind {
        MaiProviderKind::Openai => ProviderInfo::openai(Some(provider.base_url.clone())),
        MaiProviderKind::Deepseek => ProviderInfo::deepseek(Some(provider.base_url.clone())),
        MaiProviderKind::Zhipu => ProviderInfo::zhipu(Some(provider.base_url.clone())),
        MaiProviderKind::Mimo => ProviderInfo::openai_compatible_chat(
            provider.name.clone(),
            provider.base_url.clone(),
            provider.default_model.clone(),
        ),
    };
    info.name = provider.name.clone();
    info.default_model = provider.default_model.clone();
    info.bearer_token = Some(provider.api_key.clone());
    info
}

fn model_continuation_config(selection: &ProviderSelection) -> CoreModelContinuationConfig {
    CoreModelContinuationConfig::from_profile(CoreModelContinuationProfile {
        provider_family: provider_family(selection.provider.kind),
        wire_api: wire_api(selection.model.wire_api),
        model_supports_continuation: selection.model.capabilities.continuation,
        base_url: selection.provider.base_url.clone(),
        model: selection.model.id.clone(),
    })
}

fn provider_family(kind: MaiProviderKind) -> CoreModelProviderFamily {
    match kind {
        MaiProviderKind::Openai => CoreModelProviderFamily::OpenAi,
        MaiProviderKind::Deepseek | MaiProviderKind::Zhipu | MaiProviderKind::Mimo => {
            CoreModelProviderFamily::Other
        }
    }
}

fn wire_api(wire_api: ModelWireApi) -> CoreModelWireApi {
    match wire_api {
        ModelWireApi::Responses => CoreModelWireApi::Responses,
        ModelWireApi::ChatCompletions => CoreModelWireApi::Chat,
    }
}

pub(crate) fn model_info(model: &ModelConfig) -> ModelInfo {
    let mut info = ModelInfo::fallback(&model.id);
    info.display_name = model.name.clone().unwrap_or_else(|| model.id.clone());
    info.context_window = Some(model.context_tokens);
    info.max_context_window = model.max_context_tokens.or(Some(model.context_tokens));
    info.auto_compact_token_limit = model.auto_compact_token_limit;
    info.max_output_tokens = Some(model.output_tokens);
    info.capabilities = model_capabilities(&model.capabilities, model.supports_tools);
    info.capabilities.reasoning = model.reasoning.is_some();
    info.parameters = reasoning_parameters(model);
    info.request_profile = request_profile(model);
    info
}

pub(crate) fn reasoning_config(
    model: &ModelConfig,
    reasoning_effort: Option<&str>,
) -> Option<ReasoningConfig> {
    let config = model.reasoning.as_ref()?;
    let effort = reasoning_parameter(model)?
        .resolve_candidate(ModelParameterCandidateRequest {
            requested: reasoning_effort,
            default_candidate: config.default_variant.as_deref(),
            missing: MissingCandidatePolicy::UseDefault,
            disabled_values: &[],
        })
        .ok()
        .flatten();
    Some(ReasoningConfig {
        effort,
        summary: Some(ReasoningSummary::Auto),
    })
}

fn model_capabilities(
    capabilities: &MaiModelCapabilities,
    supports_tools: bool,
) -> ModelCapabilities {
    ModelCapabilities {
        streaming: true,
        temperature: true,
        reasoning: capabilities.reasoning_replay,
        web_search: false,
        input: vec![ModelModality::Text],
        output: vec![ModelModality::Text],
        tools: ToolCapabilities {
            function_calling: supports_tools && capabilities.tools,
            parallel_tool_calls: capabilities.parallel_tools,
            custom_tools: false,
            freeform_tools: false,
        },
        interleaved: None,
    }
}

fn request_profile(model: &ModelConfig) -> ModelRequestProfile {
    let mut profile = ModelRequestProfile {
        api_model: Some(model.id.clone()),
        headers: model
            .headers
            .iter()
            .chain(model.request_policy.headers.iter())
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<HashMap<_, _>>(),
        max_tokens_field: match model.request_policy.max_tokens_field.as_str() {
            "max_completion_tokens" => MaxTokensField::MaxCompletionTokens,
            "max_tokens" => MaxTokensField::MaxTokens,
            _ => MaxTokensField::MaxTokens,
        },
        ..ModelRequestProfile::default()
    };
    profile.extend_body_from_value(&model.options);
    profile.extend_body_from_value(&model.request_policy.extra_body);
    profile
}

fn reasoning_parameters(model: &ModelConfig) -> Vec<ModelParameter> {
    reasoning_parameter(model).into_iter().collect()
}

pub(crate) fn reasoning_parameter(model: &ModelConfig) -> Option<ModelParameter> {
    let Some(config) = model.reasoning.as_ref() else {
        return None;
    };
    if config.variants.is_empty() {
        return None;
    }

    let mut wire = BTreeMap::new();
    for variant in &config.variants {
        wire.insert(
            variant.id.clone(),
            ParameterWire {
                set: pl_model::wire_assignments_from_value(&variant.request),
                remove: Vec::new(),
            },
        );
    }

    Some(ModelParameter {
        name: "effort".to_string(),
        label: Some("Reasoning effort".to_string()),
        candidates: config
            .variants
            .iter()
            .map(|variant| variant.id.clone())
            .collect(),
        wire,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mai_protocol::{
        ModelCapabilities as MaiModelCapabilities, ModelConfig, ModelReasoningConfig,
        ModelReasoningVariant, ModelWireApi,
    };
    use pretty_assertions::assert_eq;
    use serde_json::{Value, json};

    use super::*;

    fn reasoning_model() -> ModelConfig {
        ModelConfig {
            id: "reasoning-model".to_string(),
            name: None,
            context_tokens: 128_000,
            max_context_tokens: None,
            effective_context_window_percent: 95,
            output_tokens: 4096,
            auto_compact_token_limit: None,
            supports_tools: true,
            wire_api: ModelWireApi::Responses,
            capabilities: MaiModelCapabilities::default(),
            request_policy: Default::default(),
            reasoning: Some(ModelReasoningConfig {
                default_variant: Some("medium".to_string()),
                variants: ["high", "medium"]
                    .into_iter()
                    .map(|id| ModelReasoningVariant {
                        id: id.to_string(),
                        label: None,
                        request: json!({
                            "reasoning": {
                                "effort": id,
                            },
                        }),
                    })
                    .collect(),
            }),
            options: Value::Null,
            headers: BTreeMap::new(),
        }
    }

    #[test]
    fn continuation_policy_delegates_to_pl_core_profile() {
        let source = include_str!("model_profile.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");

        assert!(
            production.contains("CoreModelContinuationConfig::from_profile"),
            "模型 continuation 规则应由 pl-core CoreModelContinuationConfig 统一提供"
        );
        assert!(
            production.contains("with_continuation_config"),
            "CoreModelTurnRequest 应直接消费 pl-core continuation config"
        );
        for forbidden in [
            "with_continuation(supports_continuation)",
            "with_continuation_cache_key",
            "fn continuation_cache_key",
            "selection.provider.kind == MaiProviderKind::Openai",
        ] {
            assert!(
                !production.contains(forbidden),
                "mai-runtime 不应本地维护 continuation 策略 `{forbidden}`"
            );
        }
    }

    #[test]
    fn reasoning_variant_wire_assignments_use_pl_model_flattener() {
        let source = include_str!("model_profile.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");

        assert!(
            production.contains("pl_model::wire_assignments_from_value"),
            "reasoning variant request JSON 到 WireAssignment 的拍平语义应由 pl-model 统一提供"
        );
        for forbidden in [
            "fn wire_assignments(",
            "fn collect_wire_assignments(",
            "Value::Object(object)",
            "Value::Null => {}",
        ] {
            assert!(
                !production.contains(forbidden),
                "mai-runtime 不应复制 pl-model wire assignment 拍平逻辑 `{forbidden}`"
            );
        }
    }

    #[test]
    fn request_profile_body_merge_uses_pl_model_helper() {
        let source = include_str!("model_profile.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");

        assert!(
            production.contains("extend_body_from_value"),
            "模型 request profile 的 body object 合并语义应由 pl-model ModelRequestProfile 提供"
        );
        for forbidden in ["fn merge_object(", "value.as_object()", "target.extend("] {
            assert!(
                !production.contains(forbidden),
                "mai-runtime 不应复制 request profile body 合并逻辑 `{forbidden}`"
            );
        }
    }

    #[test]
    fn reasoning_config_default_effort_uses_pl_model_candidate_resolution() {
        let source = include_str!("model_profile.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");

        assert!(
            production.contains("resolve_candidate"),
            "模型请求默认 reasoning effort 应由 pl-model ModelParameter 候选值解析统一提供"
        );
        for forbidden in [
            "fn default_reasoning_effort(",
            "variants.first().map(|variant| variant.id.clone())",
        ] {
            assert!(
                !production.contains(forbidden),
                "mai-runtime 不应复制 reasoning effort 默认候选值规则 `{forbidden}`"
            );
        }
    }

    #[test]
    fn reasoning_config_uses_configured_default_variant() {
        let model = reasoning_model();

        let config = reasoning_config(&model, None).expect("reasoning config");

        assert_eq!(config.effort, Some("medium".to_string()));
    }
}
