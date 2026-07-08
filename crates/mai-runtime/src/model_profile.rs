use std::collections::{BTreeMap, HashMap};

use mai_protocol::{
    ModelCapabilities as MaiModelCapabilities, ModelConfig, ModelReasoningVariant, ModelWireApi,
    ProviderKind as MaiProviderKind, ProviderSecret,
};
use mai_store::ProviderSelection;
use pl_core::CoreModelTurnRequest;
use pl_model::{
    MaxTokensField, ModelCapabilities, ModelInfo, ModelModality, ModelParameter,
    ModelRequestProfile, ParameterWire, ProviderInfo, ReasoningConfig, ReasoningSummary,
    SharedModelProvider, ToolCapabilities, ToolSchema, WireAssignment, create_provider_with_models,
};
use pl_protocol::PureError;
use serde_json::{Map, Value};

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
    let supports_continuation = model_supports_continuation(selection);
    let mut request = CoreModelTurnRequest::new(selection.model.id.clone())
        .with_instructions(instructions)
        .with_tools(tools)
        .with_parallel_tool_calls(selection.model.capabilities.parallel_tools)
        .with_max_tokens(Some(selection.model.output_tokens))
        .with_reasoning(reasoning_config(&selection.model, reasoning_effort))
        .with_continuation(supports_continuation);
    if supports_continuation {
        request = request.with_continuation_cache_key(continuation_cache_key(selection));
    }
    request
}

/// 判断当前 mai provider/model 选择是否能走 pl-core continuation 路径。
pub fn model_supports_continuation(selection: &ProviderSelection) -> bool {
    selection.provider.kind == MaiProviderKind::Openai
        && selection.model.wire_api == ModelWireApi::Responses
        && selection.model.capabilities.continuation
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

fn continuation_cache_key(selection: &ProviderSelection) -> String {
    format!(
        "{:?}|{}|{}",
        selection.provider.kind,
        selection.provider.base_url.trim_end_matches('/'),
        selection.model.id
    )
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
    let effort = reasoning_effort
        .map(ToString::to_string)
        .or_else(|| default_reasoning_effort(&config.variants));
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
    let mut body = Map::new();
    merge_object(&mut body, &model.options);
    merge_object(&mut body, &model.request_policy.extra_body);
    ModelRequestProfile {
        api_model: Some(model.id.clone()),
        headers: model
            .headers
            .iter()
            .chain(model.request_policy.headers.iter())
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<HashMap<_, _>>(),
        body,
        options: Map::new(),
        max_tokens_field: match model.request_policy.max_tokens_field.as_str() {
            "max_completion_tokens" => MaxTokensField::MaxCompletionTokens,
            "max_tokens" => MaxTokensField::MaxTokens,
            _ => MaxTokensField::MaxTokens,
        },
    }
}

fn merge_object(target: &mut Map<String, Value>, value: &Value) {
    if let Some(object) = value.as_object() {
        target.extend(
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
}

fn default_reasoning_effort(variants: &[ModelReasoningVariant]) -> Option<String> {
    variants.first().map(|variant| variant.id.clone())
}

fn reasoning_parameters(model: &ModelConfig) -> Vec<ModelParameter> {
    let Some(config) = model.reasoning.as_ref() else {
        return Vec::new();
    };
    if config.variants.is_empty() {
        return Vec::new();
    }

    let mut wire = BTreeMap::new();
    for variant in &config.variants {
        wire.insert(
            variant.id.clone(),
            ParameterWire {
                set: wire_assignments(&variant.request),
                remove: Vec::new(),
            },
        );
    }

    vec![ModelParameter {
        name: "effort".to_string(),
        label: Some("Reasoning effort".to_string()),
        candidates: config
            .variants
            .iter()
            .map(|variant| variant.id.clone())
            .collect(),
        wire,
    }]
}

fn wire_assignments(request: &Value) -> Vec<WireAssignment> {
    let mut assignments = Vec::new();
    collect_wire_assignments(None, request, &mut assignments);
    assignments
}

fn collect_wire_assignments(
    prefix: Option<&str>,
    value: &Value,
    assignments: &mut Vec<WireAssignment>,
) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                let path = prefix
                    .map(|prefix| format!("{prefix}.{key}"))
                    .unwrap_or_else(|| key.clone());
                collect_wire_assignments(Some(&path), value, assignments);
            }
        }
        Value::Null => {}
        Value::Array(_) | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            if let Some(path) = prefix {
                assignments.push(WireAssignment {
                    path: path.to_string(),
                    value: value.clone(),
                });
            }
        }
    }
}
