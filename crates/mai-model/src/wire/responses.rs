use crate::error::{ModelError, Result};
use crate::types::{ModelStreamEvent, ModelStreamStatus};
use crate::usage::parse_responses_usage;
use crate::wire::{SseFrame, WireProtocol, WireRequest};
use mai_protocol::{ModelInputItem, ModelOutputItem, ToolDefinition};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug)]
pub(crate) struct ResponsesApi;

#[derive(Debug, Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    instructions: &'a str,
    input: &'a [ModelInputItem],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: &'a [ToolDefinition],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
    parallel_tool_calls: bool,
    stream: bool,
    include: &'a [&'a str],
    #[serde(skip_serializing_if = "Option::is_none")]
    store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
    #[serde(skip_serializing_if = "u64_is_zero")]
    max_output_tokens: u64,
    #[serde(flatten)]
    options: BTreeMap<String, Value>,
}

fn u64_is_zero(v: &u64) -> bool {
    *v == 0
}

impl WireProtocol for ResponsesApi {
    fn path(&self) -> &'static str {
        "/responses"
    }

    fn build_body(&self, req: &WireRequest<'_>) -> Result<Vec<u8>> {
        let active_tools = if req.supports_tools { req.tools } else { &[] };
        let request = ResponsesRequest {
            model: req.model_id,
            instructions: req.instructions,
            input: req.input,
            tools: active_tools,
            tool_choice: req.tool_choice,
            parallel_tool_calls: true,
            stream: req.stream,
            include: &[],
            store: req.store,
            previous_response_id: req.previous_response_id,
            prompt_cache_key: req.prompt_cache_key,
            max_output_tokens: req.max_output_tokens,
            options: req.extra_body.clone(),
        };
        Ok(serde_json::to_vec(&request)?)
    }

    fn parse_stream_event(&self, event: &SseFrame) -> Result<Vec<ModelStreamEvent>> {
        if event.data.trim() == "[DONE]" || event.data.trim().is_empty() {
            return Ok(Vec::new());
        }
        let value: Value = serde_json::from_str(&event.data)?;
        parse_stream_event_value(value)
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

pub(crate) fn parse_stream_event_value(value: Value) -> Result<Vec<ModelStreamEvent>> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut events = Vec::new();
    match kind {
        "response.created" => events.push(ModelStreamEvent::ResponseStarted {
            id: response_id(&value),
        }),
        "response.queued" => events.push(ModelStreamEvent::Status {
            status: ModelStreamStatus::Queued,
        }),
        "response.in_progress" => events.push(ModelStreamEvent::Status {
            status: ModelStreamStatus::InProgress,
        }),
        "response.output_text.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                events.push(ModelStreamEvent::TextDelta {
                    output_index: output_index(&value),
                    content_index: content_index(&value),
                    delta: delta.to_string(),
                });
            }
        }
        "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                events.push(ModelStreamEvent::ReasoningDelta {
                    output_index: output_index(&value),
                    content_index: content_index(&value).or_else(|| {
                        value
                            .get("summary_index")
                            .and_then(Value::as_u64)
                            .map(|v| v as usize)
                    }),
                    delta: delta.to_string(),
                });
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                events.push(ModelStreamEvent::ToolCallArgumentsDelta {
                    output_index: output_index(&value),
                    delta: delta.to_string(),
                });
            }
        }
        "response.custom_tool_call_input.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                let item_id = value
                    .get("item_id")
                    .or_else(|| value.get("call_id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !item_id.is_empty() {
                    events.push(ModelStreamEvent::CustomToolInputDelta {
                        item_id,
                        call_id: value
                            .get("call_id")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                        delta: delta.to_string(),
                    });
                }
            }
        }
        "response.function_call_arguments.done" => {
            // Ordinary function calls are finalized from response.output_item.done.
            // The arguments.done event is useful as an upstream lifecycle marker, but
            // it does not consistently carry stable call metadata across providers.
        }
        "response.output_item.added" => {
            if let Some(item) = value.get("item").cloned() {
                let item = parse_output_item(item);
                events.push(ModelStreamEvent::OutputItemAdded {
                    output_index: output_index(&value),
                    item,
                });
            }
            if let Some(item) = value.get("item")
                && item.get("type").and_then(Value::as_str) == Some("function_call")
            {
                events.push(ModelStreamEvent::ToolCallStarted {
                    output_index: output_index(&value),
                    call_id: item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                });
            }
        }
        "response.output_item.done" => {
            if let Some(item) = value.get("item").cloned() {
                let item = parse_output_item(item);
                events.push(ModelStreamEvent::OutputItemDone {
                    output_index: output_index(&value),
                    item,
                });
            }
        }
        "response.completed" => {
            events.push(ModelStreamEvent::Completed {
                id: response_id(&value),
                usage: value
                    .get("response")
                    .and_then(|response| parse_responses_usage(response.get("usage"))),
                end_turn: value
                    .get("response")
                    .and_then(|response| response.get("end_turn"))
                    .and_then(Value::as_bool),
            });
        }
        "response.failed" => {
            return Err(ModelError::Stream(response_error_message(
                &value,
                "response.failed event received",
            )));
        }
        "response.incomplete" => {
            return Err(ModelError::Stream(response_error_message(
                &value,
                "response.incomplete event received",
            )));
        }
        _ => {}
    }
    Ok(events)
}

fn response_id(value: &Value) -> Option<String> {
    value
        .get("response")
        .and_then(|response| response.get("id"))
        .or_else(|| value.get("response_id"))
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn output_index(value: &Value) -> usize {
    value
        .get("output_index")
        .or_else(|| value.get("item_index"))
        .and_then(Value::as_u64)
        .unwrap_or_default() as usize
}

fn content_index(value: &Value) -> Option<usize> {
    value
        .get("content_index")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
}

fn response_error_message(value: &Value, fallback: &str) -> String {
    value
        .get("response")
        .and_then(|response| response.get("error"))
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or(fallback)
        .to_string()
}

#[cfg(test)]
fn parse_response(value: Value) -> mai_protocol::ModelResponse {
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
    let usage = parse_responses_usage(value.get("usage"));
    mai_protocol::ModelResponse { id, output, usage }
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
    use crate::provider::request_options;
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
        }));
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
            parallel_tool_calls: true,
            stream: false,
            include: &[],
            store: Some(true),
            previous_response_id,
            prompt_cache_key: Some("agent:agent-1:session:session-1"),
            max_output_tokens: 384_000,
            options: request_options(&model, None),
        };
        let value = serde_json::to_value(&request).expect("request json");

        assert_eq!(value["previous_response_id"].as_str(), Some("resp_1"));
        assert_eq!(
            value["prompt_cache_key"].as_str(),
            Some("agent:agent-1:session:session-1")
        );
        assert_eq!(value["store"].as_bool(), Some(true));
        assert_eq!(value["input"].as_array().expect("input").len(), 1);
        assert_eq!(value["input"][0]["call_id"].as_str(), Some("call_1"));
        state.acknowledge_history_len(input.len());
        assert_eq!(state.acknowledged_input_len, 3);
    }
}
