use mai_protocol::{
    ModelConfig, ModelInputItem, ModelOutputItem, ModelOutputToolCall, ModelResponse, ProviderKind,
    ProviderSecret, ReasoningEffort, TokenUsage, ToolDefinition,
};
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("request to {endpoint} failed: {source}")]
    Request {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("request to {endpoint} returned {status}: {body}")]
    Api {
        endpoint: String,
        status: StatusCode,
        body: String,
    },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, ModelError>;

#[derive(Debug, Clone)]
pub struct ResponsesClient {
    http: reqwest::Client,
}

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
    reasoning: Option<ReasoningRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    store: Option<bool>,
    #[serde(flatten)]
    options: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize)]
struct ReasoningRequest {
    effort: &'static str,
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<DeepseekThinkingRequest>,
    #[serde(flatten)]
    options: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize)]
struct DeepseekThinkingRequest {
    #[serde(rename = "type")]
    kind: &'static str,
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

impl ResponsesClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn create_response(
        &self,
        provider: &ProviderSecret,
        model: &ModelConfig,
        instructions: &str,
        input: &[ModelInputItem],
        tools: &[ToolDefinition],
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<ModelResponse> {
        match provider.kind {
            ProviderKind::Openai => {
                self.create_openai_response(
                    provider,
                    model,
                    instructions,
                    input,
                    tools,
                    reasoning_effort,
                )
                .await
            }
            ProviderKind::Deepseek => {
                self.create_deepseek_chat(
                    provider,
                    model,
                    instructions,
                    input,
                    tools,
                    reasoning_effort,
                )
                .await
            }
        }
    }

    async fn create_openai_response(
        &self,
        provider: &ProviderSecret,
        model: &ModelConfig,
        instructions: &str,
        input: &[ModelInputItem],
        tools: &[ToolDefinition],
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<ModelResponse> {
        let endpoint = format!("{}/responses", provider.base_url.trim_end_matches('/'));
        let active_tools = if model.supports_tools { tools } else { &[] };
        let request = ResponsesRequest {
            model: &model.id,
            instructions,
            input,
            tools: active_tools,
            tool_choice: (!active_tools.is_empty()).then_some("auto"),
            stream: false,
            reasoning: model
                .supports_reasoning
                .then_some(reasoning_effort.or(model.default_reasoning_effort))
                .flatten()
                .and_then(reasoning_effort_value)
                .map(|effort| ReasoningRequest { effort }),
            store: Some(false),
            options: option_map(&model.options),
        };
        let response = self
            .http
            .post(&endpoint)
            .bearer_auth(&provider.api_key)
            .headers(headers(&provider.headers(&model.headers)))
            .json(&request)
            .send()
            .await
            .map_err(|source| ModelError::Request {
                endpoint: endpoint.clone(),
                source,
            })?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|source| ModelError::Request {
                endpoint: endpoint.clone(),
                source,
            })?;
        if !status.is_success() {
            return Err(ModelError::Api {
                endpoint,
                status,
                body,
            });
        }

        parse_response(serde_json::from_str(&body)?)
    }

    async fn create_deepseek_chat(
        &self,
        provider: &ProviderSecret,
        model: &ModelConfig,
        instructions: &str,
        input: &[ModelInputItem],
        tools: &[ToolDefinition],
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<ModelResponse> {
        let endpoint = format!(
            "{}/chat/completions",
            provider.base_url.trim_end_matches('/')
        );
        let active_tools = if model.supports_tools {
            tools.iter().map(chat_tool).collect()
        } else {
            Vec::new()
        };
        let request =
            deepseek_chat_request(model, instructions, input, active_tools, reasoning_effort);
        let response = self
            .http
            .post(&endpoint)
            .bearer_auth(&provider.api_key)
            .headers(headers(&provider.headers(&model.headers)))
            .json(&request)
            .send()
            .await
            .map_err(|source| ModelError::Request {
                endpoint: endpoint.clone(),
                source,
            })?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|source| ModelError::Request {
                endpoint: endpoint.clone(),
                source,
            })?;
        if !status.is_success() {
            return Err(ModelError::Api {
                endpoint,
                status,
                body,
            });
        }

        parse_chat_response(serde_json::from_str(&body)?)
    }
}

impl Default for ResponsesClient {
    fn default() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(600))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { http }
    }
}

trait HeaderMerge {
    fn headers(&self, model_headers: &BTreeMap<String, String>) -> BTreeMap<String, String>;
}

impl HeaderMerge for ProviderSecret {
    fn headers(&self, model_headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::new();
        headers.extend(model_headers.clone());
        headers
    }
}

fn headers(values: &BTreeMap<String, String>) -> reqwest::header::HeaderMap {
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

fn option_map(value: &Value) -> BTreeMap<String, Value> {
    value
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn reasoning_effort_value(effort: ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::None => None,
        ReasoningEffort::Minimal => Some("minimal"),
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High => Some("high"),
        ReasoningEffort::Xhigh => Some("xhigh"),
        ReasoningEffort::Max => Some("max"),
    }
}

fn deepseek_thinking_type(effort: ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::High | ReasoningEffort::Max => Some("enabled"),
        _ => None,
    }
}

fn deepseek_max_tokens(configured: u64) -> u64 {
    configured.clamp(1, 64_000)
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

fn deepseek_chat_request(
    model: &ModelConfig,
    instructions: &str,
    input: &[ModelInputItem],
    tools: Vec<ChatTool>,
    reasoning_effort: Option<ReasoningEffort>,
) -> ChatRequest {
    let thinking = model
        .supports_reasoning
        .then_some(reasoning_effort.or(model.default_reasoning_effort))
        .flatten()
        .and_then(deepseek_thinking_type)
        .map(|kind| DeepseekThinkingRequest { kind });
    ChatRequest {
        model: model.id.clone(),
        messages: chat_messages(instructions, input),
        tool_choice: (!tools.is_empty()).then_some("auto"),
        tools,
        stream: false,
        max_tokens: deepseek_max_tokens(model.output_tokens),
        thinking,
        options: option_map(&model.options),
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
                        mai_protocol::ModelContentItem::InputText { text }
                        | mai_protocol::ModelContentItem::OutputText { text } => text.as_str(),
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
            } => messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: content.clone(),
                reasoning_content: last_user_index
                    .is_none_or(|last_user_index| index > last_user_index)
                    .then(|| reasoning_content.clone())
                    .flatten(),
                tool_calls: tool_calls
                    .iter()
                    .map(|tool_call| ChatToolCall {
                        id: tool_call.call_id.clone(),
                        kind: "function",
                        function: ChatFunctionCall {
                            name: tool_call.name.clone(),
                            arguments: tool_call.arguments.clone(),
                        },
                    })
                    .collect(),
                tool_call_id: None,
            }),
            ModelInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: None,
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

fn parse_usage(value: Option<&Value>) -> Option<TokenUsage> {
    let value = value?;
    let input_tokens = value
        .get("input_tokens")
        .or_else(|| value.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output_tokens = value
        .get("output_tokens")
        .or_else(|| value.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let total_tokens = value
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens + output_tokens);
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    tool_calls: vec![mai_protocol::ModelToolCall {
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
        let model = ModelConfig {
            id: "deepseek-v4-pro".to_string(),
            name: Some("DeepSeek V4 Pro".to_string()),
            context_tokens: 1_000_000,
            output_tokens: 384_000,
            supports_tools: true,
            supports_reasoning: true,
            reasoning_efforts: vec![ReasoningEffort::High, ReasoningEffort::Max],
            default_reasoning_effort: Some(ReasoningEffort::High),
            options: serde_json::Value::Null,
            headers: BTreeMap::new(),
        };
        let request = deepseek_chat_request(
            &model,
            "instructions",
            &[ModelInputItem::user_text("hello")],
            Vec::new(),
            Some(ReasoningEffort::High),
        );

        assert_eq!(request.max_tokens, 64_000);
        assert_eq!(
            request.thinking.as_ref().map(|item| item.kind),
            Some("enabled")
        );
    }
}
