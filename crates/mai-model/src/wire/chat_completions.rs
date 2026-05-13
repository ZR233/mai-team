use crate::error::Result;
use crate::wire::{WireProtocol, WireRequest, parse_usage};
use mai_protocol::{
    ModelContentItem, ModelInputItem, ModelOutputItem, ModelOutputToolCall, ModelResponse,
    ToolDefinition,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug)]
pub(crate) struct ChatCompletionsApi;

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    tools: Vec<ChatTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    stream: bool,
    max_tokens: u64,
    #[serde(flatten)]
    options: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    tool_calls: Vec<ChatToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: ChatFunctionCall,
}

#[derive(Debug, Serialize)]
struct ChatFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct ChatTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ChatToolFunction,
}

#[derive(Debug, Serialize)]
struct ChatToolFunction {
    name: String,
    description: String,
    parameters: Value,
}

impl WireProtocol for ChatCompletionsApi {
    fn path(&self) -> &'static str {
        "/chat/completions"
    }

    fn build_body(&self, req: &WireRequest<'_>) -> Result<Vec<u8>> {
        let active_tools: Vec<ChatTool> = if req.supports_tools {
            req.tools.iter().map(chat_tool).collect()
        } else {
            Vec::new()
        };
        let request = ChatRequest {
            model: req.model_id.to_string(),
            messages: chat_messages(req.instructions, req.input),
            tool_choice: (!active_tools.is_empty()).then_some("auto"),
            tools: active_tools,
            stream: req.stream,
            max_tokens: req.max_output_tokens,
            options: req.extra_body.clone(),
        };
        Ok(serde_json::to_vec(&request)?)
    }

    fn parse_response(&self, body: &str) -> Result<ModelResponse> {
        let value: Value = serde_json::from_str(body)?;
        parse_chat_response(value)
    }
}

fn chat_tool(tool: &ToolDefinition) -> ChatTool {
    ChatTool {
        kind: "function",
        function: ChatToolFunction {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
        },
    }
}

fn chat_messages(instructions: &str, input: &[ModelInputItem]) -> Vec<ChatMessage> {
    let last_user_index = input
        .iter()
        .rposition(|item| matches!(item, ModelInputItem::Message { role, .. } if role == "user"));
    let mut messages = vec![ChatMessage {
        role: "system".to_string(),
        content: Some(instructions.to_string()),
        reasoning_content: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
    }];
    for (index, item) in input.iter().enumerate() {
        match item {
            ModelInputItem::Message { role, content } => {
                let text = content
                    .iter()
                    .map(|item| match item {
                        ModelContentItem::InputText { text }
                        | ModelContentItem::OutputText { text } => text.as_str(),
                    })
                    .collect::<Vec<_>>()
                    .join("");
                messages.push(ChatMessage {
                    role: role.clone(),
                    content: Some(text),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                });
            }
            ModelInputItem::AssistantTurn {
                content,
                reasoning_content,
                tool_calls,
            } => {
                let tool_calls = tool_calls
                    .iter()
                    .map(|tool_call| ChatToolCall {
                        id: tool_call.call_id.clone(),
                        kind: "function",
                        function: ChatFunctionCall {
                            name: tool_call.name.clone(),
                            arguments: tool_call.arguments.clone(),
                        },
                    })
                    .collect::<Vec<_>>();
                let reasoning_content = last_user_index
                    .is_none_or(|last_user_index| index > last_user_index)
                    .then(|| reasoning_content.clone())
                    .flatten();
                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: assistant_chat_content(content, &tool_calls, reasoning_content.as_deref()),
                    reasoning_content,
                    tool_calls,
                    tool_call_id: None,
                });
            }
            ModelInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: Some(String::new()),
                reasoning_content: None,
                tool_calls: vec![ChatToolCall {
                    id: call_id.clone(),
                    kind: "function",
                    function: ChatFunctionCall {
                        name: name.clone(),
                        arguments: arguments.clone(),
                    },
                }],
                tool_call_id: None,
            }),
            ModelInputItem::FunctionCallOutput { call_id, output } => messages.push(ChatMessage {
                role: "tool".to_string(),
                content: Some(output.clone()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                tool_call_id: Some(call_id.clone()),
            }),
        }
    }
    messages
}

fn assistant_chat_content(
    content: &Option<String>,
    tool_calls: &[ChatToolCall],
    reasoning_content: Option<&str>,
) -> Option<String> {
    content
        .clone()
        .or_else(|| (!tool_calls.is_empty() || reasoning_content.is_some()).then(String::new))
}

fn parse_chat_response(value: Value) -> Result<ModelResponse> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let mut output = Vec::new();
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        for choice in choices {
            let Some(message) = choice.get("message") else {
                continue;
            };
            let content = message
                .get("content")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .filter(|text| !text.is_empty());
            let reasoning_content = message
                .get("reasoning_content")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .filter(|reasoning| !reasoning.trim().is_empty());
            let mut tool_calls = Vec::new();
            if let Some(raw_tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                for tool_call in raw_tool_calls {
                    let function = tool_call.get("function").unwrap_or(&Value::Null);
                    let raw_arguments = function
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}")
                        .to_string();
                    let arguments = serde_json::from_str(&raw_arguments)
                        .unwrap_or_else(|_| json!({ "raw": raw_arguments.clone() }));
                    tool_calls.push(ModelOutputToolCall {
                        call_id: tool_call
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: function
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        arguments,
                        raw_arguments,
                    });
                }
            }
            if reasoning_content.is_some() {
                output.push(ModelOutputItem::AssistantTurn {
                    content,
                    reasoning_content,
                    tool_calls,
                });
            } else {
                if let Some(text) = content {
                    output.push(ModelOutputItem::Message { text });
                }
                output.extend(tool_calls.into_iter().map(|tool_call| {
                    ModelOutputItem::FunctionCall {
                        call_id: tool_call.call_id,
                        name: tool_call.name,
                        arguments: tool_call.arguments,
                        raw_arguments: tool_call.raw_arguments,
                    }
                }));
            }
        }
    }
    let usage = parse_usage(value.get("usage"));
    Ok(ModelResponse { id, output, usage })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{ModelReasoningConfig, ModelReasoningVariant, ModelToolCall, ModelWireApi};

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
            wire_api: ModelWireApi::ChatCompletions,
            capabilities: Default::default(),
            request_policy: Default::default(),
        }
    }

    fn deepseek_model() -> mai_protocol::ModelConfig {
        model_with_reasoning("deepseek-v4-pro", &["high", "max"], "high", |id| {
            json!({
                "thinking": {
                    "type": "enabled",
                },
                "reasoning_effort": id,
            })
        })
    }

    #[test]
    fn parses_chat_message_reasoning_tool_calls_and_usage() {
        let response = parse_chat_response(json!({
            "id": "chat_1",
            "choices": [
                {
                    "message": {
                        "content": "hello",
                        "reasoning_content": "thinking",
                        "tool_calls": [
                            {
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "container_exec",
                                    "arguments": "{\"command\":\"pwd\"}"
                                }
                            }
                        ]
                    }
                }
            ],
            "usage": { "prompt_tokens": 4, "completion_tokens": 5, "total_tokens": 9 }
        }))
        .expect("parse");

        assert_eq!(response.id.as_deref(), Some("chat_1"));
        assert_eq!(response.output.len(), 1);
        assert_eq!(response.usage.expect("usage").total_tokens, 9);
        assert!(matches!(
            &response.output[0],
            ModelOutputItem::AssistantTurn {
                content: Some(content),
                reasoning_content: Some(reasoning),
                tool_calls,
            } if content == "hello"
                && reasoning == "thinking"
                && tool_calls.len() == 1
                && tool_calls[0].call_id == "call_1"
                && tool_calls[0].name == "container_exec"
        ));
    }

    #[test]
    fn chat_messages_preserve_reasoning_content_for_assistant_turns() {
        let messages = chat_messages(
            "instructions",
            &[
                ModelInputItem::user_text("hello"),
                ModelInputItem::AssistantTurn {
                    content: None,
                    reasoning_content: Some("thinking".to_string()),
                    tool_calls: vec![ModelToolCall {
                        call_id: "call_1".to_string(),
                        name: "container_exec".to_string(),
                        arguments: "{\"command\":\"pwd\"}".to_string(),
                    }],
                },
            ],
        );

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(messages[2].reasoning_content.as_deref(), Some("thinking"));
        assert_eq!(messages[2].tool_calls[0].id, "call_1");
    }

    #[test]
    fn chat_messages_drop_stale_reasoning_content_before_latest_user_message() {
        let messages = chat_messages(
            "instructions",
            &[
                ModelInputItem::user_text("first"),
                ModelInputItem::AssistantTurn {
                    content: None,
                    reasoning_content: Some("old thinking".to_string()),
                    tool_calls: Vec::new(),
                },
                ModelInputItem::user_text("second"),
            ],
        );

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(messages[2].reasoning_content, None);
    }

    #[test]
    fn deepseek_request_uses_current_thinking_param_and_clamps_max_tokens() {
        let model = deepseek_model();
        let api = ChatCompletionsApi;
        let body = api
            .build_body(&WireRequest {
                model_id: &model.id,
                instructions: "instructions",
                input: &[ModelInputItem::user_text("hello")],
                tools: &[],
                tool_choice: None,
                stream: false,
                store: None,
                previous_response_id: None,
                max_output_tokens: 64_000,
                extra_body: crate::http::request_options(&model, Some("high")),
                supports_tools: true,
            })
            .expect("build");
        let value: Value = serde_json::from_slice(&body).expect("parse");

        assert_eq!(value["max_tokens"].as_u64(), Some(64_000));
        assert_eq!(
            value.pointer("/thinking/type").and_then(Value::as_str),
            Some("enabled")
        );
        assert_eq!(
            value.get("reasoning_effort").and_then(Value::as_str),
            Some("high")
        );
    }

    #[test]
    fn deepseek_reasoning_tool_call_messages_have_content_and_effort() {
        let model = deepseek_model();
        let api = ChatCompletionsApi;
        let body = api
            .build_body(&WireRequest {
                model_id: &model.id,
                instructions: "instructions",
                input: &[
                    ModelInputItem::user_text("continue"),
                    ModelInputItem::AssistantTurn {
                        content: None,
                        reasoning_content: Some("need a tool".to_string()),
                        tool_calls: vec![ModelToolCall {
                            call_id: "call_1".to_string(),
                            name: "container_exec".to_string(),
                            arguments: "{\"command\":\"pwd\"}".to_string(),
                        }],
                    },
                    ModelInputItem::FunctionCallOutput {
                        call_id: "call_1".to_string(),
                        output: "{\"status\":0,\"stdout\":\"/workspace\"}".to_string(),
                    },
                ],
                tools: &[ToolDefinition {
                    kind: "function".to_string(),
                    name: "container_exec".to_string(),
                    description: "run a command".to_string(),
                    parameters: json!({ "type": "object" }),
                }],
                tool_choice: Some("auto"),
                stream: false,
                store: None,
                previous_response_id: None,
                max_output_tokens: 64_000,
                extra_body: crate::http::request_options(&model, Some("max")),
                supports_tools: true,
            })
            .expect("build");
        let value: Value = serde_json::from_slice(&body).expect("parse");

        assert_eq!(
            value.get("reasoning_effort").and_then(Value::as_str),
            Some("max")
        );
        assert_eq!(
            value.pointer("/thinking/type").and_then(Value::as_str),
            Some("enabled")
        );
        let messages = value.get("messages").and_then(Value::as_array).expect("messages");
        let assistant_msg = &messages[2];
        assert_eq!(assistant_msg["role"].as_str(), Some("assistant"));
        assert_eq!(assistant_msg["content"].as_str(), Some(""));
        assert_eq!(
            assistant_msg["reasoning_content"].as_str(),
            Some("need a tool")
        );
        let tool_calls = assistant_msg
            .get("tool_calls")
            .and_then(Value::as_array)
            .expect("tool_calls");
        assert_eq!(tool_calls.len(), 1);
    }

    #[test]
    fn mimo_request_policy_is_independent_from_deepseek_replay() {
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
        model.id = "mimo-v2.5-pro".to_string();
        model.wire_api = ModelWireApi::ChatCompletions;
        model.capabilities.reasoning_replay = false;
        model.request_policy.extra_body = json!({ "mimo_only": true });

        let api = ChatCompletionsApi;
        let body = api
            .build_body(&WireRequest {
                model_id: &model.id,
                instructions: "instructions",
                input: &[ModelInputItem::user_text("hello")],
                tools: &[],
                tool_choice: None,
                stream: false,
                store: None,
                previous_response_id: None,
                max_output_tokens: 131_072,
                extra_body: crate::http::request_options(&model, None),
                supports_tools: true,
            })
            .expect("build");
        let value: Value = serde_json::from_slice(&body).expect("parse");

        assert_eq!(value.get("mimo_only").and_then(Value::as_bool), Some(true));
        assert!(value.get("thinking").is_none());
    }
}
