use crate::error::Result;
use crate::wire::{WireProtocol, WireRequest, parse_usage};
use mai_protocol::{ModelInputItem, ModelOutputItem, ModelResponse, ToolDefinition};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct ResponsesApi;

#[derive(Debug, Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    instructions: &'a str,
    input: &'a [ModelInputItem],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: &'a [ToolDefinition],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<&'a str>,
    #[serde(flatten)]
    options: BTreeMap<String, Value>,
}

impl WireProtocol for ResponsesApi {
    fn path(&self) -> &'static str {
        "/responses"
    }

    fn build_body(&self, req: &WireRequest<'_>) -> Result<Vec<u8>> {
        let active_tools = if req.supports_tools {
            req.tools
        } else {
            &[]
        };
        let request = ResponsesRequest {
            model: req.model_id,
            instructions: req.instructions,
            input: req.input,
            tools: active_tools,
            tool_choice: req.tool_choice,
            stream: req.stream,
            store: req.store,
            previous_response_id: req.previous_response_id,
            options: req.extra_body.clone(),
        };
        Ok(serde_json::to_vec(&request)?)
    }

    fn parse_response(&self, body: &str) -> Result<ModelResponse> {
        let value: Value = serde_json::from_str(body)?;
        parse_response(value)
    }
}

pub(crate) fn openai_turn_input<'a>(
    input: &'a [ModelInputItem],
    state: &'a crate::types::ModelTurnState,
) -> (&'a [ModelInputItem], Option<&'a str>) {
    let previous_response_id = state.previous_response_id.as_deref();
    let input = if previous_response_id.is_some() {
        let start = state.acknowledged_input_len.min(input.len());
        &input[start..]
    } else {
        input
    };
    (input, previous_response_id)
}

fn parse_response(value: Value) -> Result<ModelResponse> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(parse_output_item)
        .collect::<Vec<_>>();
    let usage = parse_usage(value.get("usage"));
    Ok(ModelResponse { id, output, usage })
}

fn parse_output_item(value: Value) -> ModelOutputItem {
    match value.get("type").and_then(Value::as_str) {
        Some("message") => {
            let text = value
                .get("content")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            item.get("text")
                                .or_else(|| item.get("output_text"))
                                .and_then(Value::as_str)
                        })
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            ModelOutputItem::Message { text }
        }
        Some("function_call") => {
            let call_id = value
                .get("call_id")
                .or_else(|| value.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let raw_arguments = value
                .get("arguments")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "{}".to_string());
            let arguments = serde_json::from_str(&raw_arguments)
                .unwrap_or_else(|_| json!({ "raw": raw_arguments.clone() }));
            ModelOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                raw_arguments,
            }
        }
        _ => ModelOutputItem::Other { raw: value },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::request_options;
    use crate::types::ModelTurnState;
    use mai_protocol::{ModelReasoningConfig, ModelReasoningVariant, ModelWireApi};

    fn model_with_reasoning(
        id: &str,
        variants: &[&str],
        default_variant: &str,
        request_for: impl Fn(&str) -> Value,
    ) -> mai_protocol::ModelConfig {
        mai_protocol::ModelConfig {
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

    fn openai_model() -> mai_protocol::ModelConfig {
        let mut model = model_with_reasoning(
            "gpt-5.5",
            &["minimal", "low", "medium", "high", "xhigh"],
            "medium",
            |id| {
                json!({
                    "reasoning": {
                        "effort": id,
                    },
                })
            },
        );
        model.capabilities.continuation = true;
        model.request_policy.store = Some(true);
        model
    }

    #[test]
    fn parses_message_and_function_call() {
        let response = parse_response(json!({
            "id": "resp_1",
            "output": [
                {
                    "type": "message",
                    "content": [{ "type": "output_text", "text": "hello" }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "container_exec",
                    "arguments": "{\"command\":\"pwd\"}"
                }
            ],
            "usage": { "input_tokens": 1, "output_tokens": 2, "total_tokens": 3 }
        }))
        .expect("parse");
        assert_eq!(response.output.len(), 2);
        assert_eq!(response.usage.expect("usage").total_tokens, 3);
    }

    #[test]
    fn openai_turn_request_uses_previous_response_id_and_delta_input() {
        let model = openai_model();
        let input = vec![
            ModelInputItem::user_text("do work"),
            ModelInputItem::FunctionCall {
                call_id: "call_1".to_string(),
                name: "container_exec".to_string(),
                arguments: "{\"command\":\"pwd\"}".to_string(),
            },
            ModelInputItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: "{\"status\":0}".to_string(),
            },
        ];
        let mut state = ModelTurnState {
            previous_response_id: Some("resp_1".to_string()),
            acknowledged_input_len: 2,
            ..Default::default()
        };
        let (request_input, previous_response_id) = openai_turn_input(&input, &state);
        let request = ResponsesRequest {
            model: &model.id,
            instructions: "instructions",
            input: request_input,
            tools: &[],
            tool_choice: None,
            stream: false,
            store: Some(true),
            previous_response_id,
            options: request_options(&model, None),
        };
        let value = serde_json::to_value(&request).expect("request json");

        assert_eq!(value["previous_response_id"].as_str(), Some("resp_1"));
        assert_eq!(value["store"].as_bool(), Some(true));
        assert_eq!(value["input"].as_array().expect("input").len(), 1);
        assert_eq!(value["input"][0]["call_id"].as_str(), Some("call_1"));
        state.acknowledge_history_len(input.len());
        assert_eq!(state.acknowledged_input_len, 3);
    }
}
