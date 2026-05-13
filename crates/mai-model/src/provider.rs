use crate::error::ModelError;
use crate::wire::chat_completions::ChatCompletionsApi;
use crate::wire::responses::ResponsesApi;
use crate::wire::WireProtocol;
use mai_protocol::{ModelConfig, ModelWireApi, ProviderKind, ProviderSecret};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::Arc;

pub struct ResolvedProvider {
    pub endpoint: String,
    pub api_key: String,
    pub headers: reqwest::header::HeaderMap,
    pub(crate) wire_protocol: Arc<dyn WireProtocol>,
    pub model_id: String,
    pub max_output_tokens: u64,
    pub supports_tools: bool,
    pub supports_continuation: bool,
    pub extra_body: BTreeMap<String, Value>,
    pub cache_key: String,
}

pub trait ProviderResolver: Debug + Send + Sync {
    fn resolve(
        &self,
        provider: &ProviderSecret,
        model: &ModelConfig,
        reasoning_effort: Option<&str>,
    ) -> ResolvedProvider;
}

#[derive(Debug)]
pub(crate) struct DefaultProviderResolver {
    responses: Arc<ResponsesApi>,
    chat_completions: Arc<ChatCompletionsApi>,
}

impl DefaultProviderResolver {
    pub(crate) fn new() -> Self {
        Self {
            responses: Arc::new(ResponsesApi),
            chat_completions: Arc::new(ChatCompletionsApi),
        }
    }
}

impl Default for DefaultProviderResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderResolver for DefaultProviderResolver {
    fn resolve(
        &self,
        provider: &ProviderSecret,
        model: &ModelConfig,
        reasoning_effort: Option<&str>,
    ) -> ResolvedProvider {
        let wire_api = effective_wire_api(provider, model);
        let wire_protocol: Arc<dyn WireProtocol> = match wire_api {
            ModelWireApi::Responses => Arc::clone(&self.responses) as Arc<dyn WireProtocol>,
            ModelWireApi::ChatCompletions => {
                Arc::clone(&self.chat_completions) as Arc<dyn WireProtocol>
            }
        };
        let endpoint = format!(
            "{}{}",
            provider.base_url.trim_end_matches('/'),
            wire_protocol.path()
        );
        let model_headers = request_headers(model);
        let headers = build_headers(&model_headers);
        let supports_tools = model.supports_tools && model.capabilities.tools;
        let supports_continuation = wire_api == ModelWireApi::Responses
            && provider.kind == ProviderKind::Openai
            && model.capabilities.continuation;

        ResolvedProvider {
            endpoint,
            api_key: provider.api_key.clone(),
            headers,
            wire_protocol,
            model_id: model.id.clone(),
            max_output_tokens: resolve_max_tokens(provider, model),
            supports_tools,
            supports_continuation,
            extra_body: request_options(model, reasoning_effort),
            cache_key: continuation_cache_key(provider, model),
        }
    }
}

pub(crate) fn response_id_unsupported_for_responses_http(err: &ModelError) -> bool {
    let ModelError::Api { status, body, .. } = err else {
        return false;
    };
    *status == reqwest::StatusCode::BAD_REQUEST
        && body.contains("previous_response_id")
        && body.contains("only supported on Responses WebSocket")
}

fn effective_wire_api(provider: &ProviderSecret, model: &ModelConfig) -> ModelWireApi {
    match provider.kind {
        ProviderKind::Openai => model.wire_api,
        ProviderKind::Deepseek | ProviderKind::Mimo => ModelWireApi::ChatCompletions,
    }
}

fn resolve_max_tokens(provider: &ProviderSecret, model: &ModelConfig) -> u64 {
    match provider.kind {
        ProviderKind::Deepseek => model.output_tokens.clamp(1, 64_000),
        ProviderKind::Mimo => model.output_tokens.clamp(1, 131_072),
        _ => model.output_tokens,
    }
}

fn continuation_cache_key(provider: &ProviderSecret, model: &ModelConfig) -> String {
    format!(
        "{:?}|{}|{}",
        provider.kind,
        provider.base_url.trim_end_matches('/'),
        model.id
    )
}

fn request_headers(model: &ModelConfig) -> BTreeMap<String, String> {
    let mut headers = model.headers.clone();
    headers.extend(model.request_policy.headers.clone());
    headers
}

fn build_headers(values: &BTreeMap<String, String>) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (key, value) in values {
        if let Ok(name) = reqwest::header::HeaderName::from_bytes(key.as_bytes())
            && let Ok(value) = reqwest::header::HeaderValue::from_str(value)
        {
            out.insert(name, value);
        }
    }
    out
}

pub(crate) fn request_options(
    model: &ModelConfig,
    reasoning_effort: Option<&str>,
) -> BTreeMap<String, Value> {
    let mut options = model.options.clone();
    if let Some(request) = reasoning_variant_request(model, reasoning_effort) {
        merge_json_objects(&mut options, request);
    }
    merge_json_objects(&mut options, &model.request_policy.extra_body);
    value_to_map(&options)
}

fn value_to_map(value: &Value) -> BTreeMap<String, Value> {
    value
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn reasoning_variant_request<'a>(
    model: &'a ModelConfig,
    reasoning_effort: Option<&str>,
) -> Option<&'a Value> {
    let reasoning = model.reasoning.as_ref()?;
    let variant_id = reasoning_effort
        .filter(|value| !value.trim().is_empty())
        .or(reasoning.default_variant.as_deref())?;
    reasoning
        .variants
        .iter()
        .find(|variant| variant.id == variant_id)
        .map(|variant| &variant.request)
}

fn merge_json_objects(base: &mut Value, overlay: &Value) {
    let Some(overlay) = overlay.as_object() else {
        return;
    };
    if !base.is_object() {
        *base = json!({});
    }
    let Some(base_map) = base.as_object_mut() else {
        return;
    };
    for (key, overlay_value) in overlay {
        match (base_map.get_mut(key), overlay_value) {
            (Some(base_value), Value::Object(_)) if base_value.is_object() => {
                merge_json_objects(base_value, overlay_value);
            }
            _ => {
                base_map.insert(key.clone(), overlay_value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{ModelReasoningConfig, ModelReasoningVariant, ModelWireApi};
    use serde_json::json;

    fn model_with_reasoning(
        id: &str,
        variants: &[&str],
        default_variant: &str,
        request_for: impl Fn(&str) -> Value,
    ) -> ModelConfig {
        ModelConfig {
            id: id.to_string(),
            name: Some(id.to_string()),
            context_tokens: 1_000_000,
            output_tokens: 384_000,
            supports_tools: true,
            reasoning: Some(ModelReasoningConfig {
                default_variant: Some(default_variant.to_string()),
                variants: variants
                    .iter()
                    .map(|id| ModelReasoningVariant {
                        id: (*id).to_string(),
                        label: None,
                        request: request_for(id),
                    })
                    .collect(),
            }),
            options: Value::Null,
            headers: BTreeMap::new(),
            wire_api: ModelWireApi::Responses,
            capabilities: Default::default(),
            request_policy: Default::default(),
        }
    }

    #[test]
    fn reasoning_variant_request_deep_merges_over_model_options() {
        let mut model = model_with_reasoning(
            "gpt-5.5",
            &["minimal", "low", "medium", "high", "xhigh"],
            "medium",
            |id| {
                json!({
                    "reasoning": {
                        "effort": id,
                        "summary": "auto"
                    },
                })
            },
        );
        model.options = json!({
            "temperature": 0.2,
            "reasoning": {
                "effort": "low",
                "summary": "auto"
            }
        });

        let options = request_options(&model, Some("xhigh"));
        assert_eq!(
            options.get("reasoning"),
            Some(&json!({
                "effort": "xhigh",
                "summary": "auto"
            }))
        );
        assert_eq!(options.get("temperature"), Some(&json!(0.2)));
    }
}
