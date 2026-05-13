use crate::error::ModelError;
use mai_protocol::{ModelConfig, ProviderSecret};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(crate) fn model_supports_tools(model: &ModelConfig) -> bool {
    model.supports_tools && model.capabilities.tools
}

pub(crate) fn response_id_unsupported_for_responses_http(err: &ModelError) -> bool {
    let ModelError::Api { status, body, .. } = err else {
        return false;
    };
    *status == reqwest::StatusCode::BAD_REQUEST
        && body.contains("previous_response_id")
        && body.contains("only supported on Responses WebSocket")
}

pub(crate) fn continuation_cache_key(provider: &ProviderSecret, model: &ModelConfig) -> String {
    format!(
        "{:?}|{}|{}",
        provider.kind,
        provider.base_url.trim_end_matches('/'),
        model.id
    )
}

pub(crate) trait HeaderMerge {
    fn headers(&self, model_headers: &BTreeMap<String, String>) -> BTreeMap<String, String>;
}

impl HeaderMerge for ProviderSecret {
    fn headers(&self, model_headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::new();
        headers.extend(model_headers.clone());
        headers
    }
}

pub(crate) fn request_headers(model: &ModelConfig) -> BTreeMap<String, String> {
    let mut headers = model.headers.clone();
    headers.extend(model.request_policy.headers.clone());
    headers
}

pub(crate) fn headers(values: &BTreeMap<String, String>) -> reqwest::header::HeaderMap {
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

pub(crate) fn option_map(value: &Value) -> BTreeMap<String, Value> {
    value
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
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
    option_map(&options)
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
    use std::collections::BTreeMap;

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
