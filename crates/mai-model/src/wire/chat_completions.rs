use crate::error::Result;
use crate::types::ModelStreamEvent;
use crate::usage::{parse_chat_usage, parse_deepseek_chat_usage};
use crate::wire::{SseFrame, WireProtocol, WireRequest};
use mai_protocol::{ModelContentItem, ModelInputItem, ModelOutputItem, ToolDefinition};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug)]
pub(crate) struct ChatCompletionsApi {
    usage_parser: ChatUsageParser,
}

#[derive(Debug, Clone, Copy)]
enum ChatUsageParser {
    OpenAiCompatible,
    Deepseek,
}

impl ChatCompletionsApi {
    pub(crate) fn openai_compatible() -> Self {
        Self {
            usage_parser: ChatUsageParser::OpenAiCompatible,
        }
    }

    pub(crate) fn deepseek() -> Self {
        Self {
            usage_parser: ChatUsageParser::Deepseek,
        }
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<ChatStreamOptions>,
    #[serde(flatten)]
    options: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize)]
struct ChatStreamOptions {
    include_usage: bool,
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
            stream_options: req.stream.then_some(ChatStreamOptions {
                include_usage: true,
            }),
            options: chat_options(req),
        };
        Ok(serde_json::to_vec(&request)?)
    }

    fn parse_stream_event(&self, event: &SseFrame) -> Result<Vec<ModelStreamEvent>> {
        if event.data.trim() == "[DONE]" || event.data.trim().is_empty() {
            return Ok(Vec::new());
        }
        let value: Value = serde_json::from_str(&event.data)?;
        Ok(parse_chat_stream_chunk_with_usage_parser(
            value,
            self.usage_parser,
        ))
    }

    fn parse_stream_done(&self) -> Result<Vec<ModelStreamEvent>> {
        Ok(vec![ModelStreamEvent::Completed {
            id: None,
            usage: None,
            end_turn: None,
        }])
    }
}

fn chat_options(req: &WireRequest<'_>) -> BTreeMap<String, Value> {
    let mut options = req.extra_body.clone();
    let max_tokens_field = if req.max_tokens_field.trim().is_empty() {
        "max_tokens"
    } else {
        req.max_tokens_field
    };
    options.insert(
        max_tokens_field.to_string(),
        Value::from(req.max_output_tokens),
    );
    options
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
    let mut messages = vec![ChatMessage {
        role: "system".to_string(),
        content: Some(instructions.to_string()),
        reasoning_content: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
    }];
    let mut assistant_replay = AssistantReplayBuilder::default();
    for item in input.iter() {
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
                if role == "assistant" {
                    assistant_replay.push_text(text);
                    continue;
                }
                assistant_replay.flush(&mut messages);
                messages.push(ChatMessage {
                    role: role.clone(),
                    content: Some(text),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                });
            }
            ModelInputItem::Reasoning { content } => {
                assistant_replay.push_reasoning(content);
            }
            ModelInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                assistant_replay.push_tool_call(ChatToolCall {
                    id: call_id.clone(),
                    kind: "function",
                    function: ChatFunctionCall {
                        name: name.clone(),
                        arguments: arguments.clone(),
                    },
                });
            }
            ModelInputItem::FunctionCallOutput { call_id, output } => {
                assistant_replay.flush(&mut messages);
                messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: Some(output.clone()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    tool_call_id: Some(call_id.clone()),
                });
            }
        }
    }
    assistant_replay.flush(&mut messages);
    messages
}

#[derive(Default)]
struct AssistantReplayBuilder {
    reasoning_content: Option<String>,
    content: Option<String>,
    tool_calls: Vec<ChatToolCall>,
}

impl AssistantReplayBuilder {
    fn push_reasoning(&mut self, content: &str) {
        if content.trim().is_empty() {
            return;
        }
        match &mut self.reasoning_content {
            Some(existing) => existing.push_str(content),
            None => self.reasoning_content = Some(content.to_string()),
        }
    }

    fn push_tool_call(&mut self, tool_call: ChatToolCall) {
        self.tool_calls.push(tool_call);
    }

    fn push_text(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        match &mut self.content {
            Some(existing) => existing.push_str(&text),
            None => self.content = Some(text),
        }
    }

    fn flush(&mut self, messages: &mut Vec<ChatMessage>) {
        if self.reasoning_content.is_none() && self.content.is_none() && self.tool_calls.is_empty()
        {
            return;
        }
        messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: Some(self.content.take().unwrap_or_default()),
            reasoning_content: self.reasoning_content.take(),
            tool_calls: std::mem::take(&mut self.tool_calls),
            tool_call_id: None,
        });
    }
}

#[cfg(test)]
fn parse_chat_stream_chunk(value: Value) -> Vec<ModelStreamEvent> {
    parse_chat_stream_chunk_with_usage_parser(value, ChatUsageParser::OpenAiCompatible)
}

fn parse_chat_stream_chunk_with_usage_parser(
    value: Value,
    usage_parser: ChatUsageParser,
) -> Vec<ModelStreamEvent> {
    let mut events = Vec::new();
    let mut tool_calls_completed = Vec::new();
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    if id.is_some() {
        events.push(ModelStreamEvent::ResponseStarted { id: id.clone() });
    }
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        for choice in choices {
            let output_index = choice
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or_default() as usize;
            let finish_reason = choice.get("finish_reason").and_then(Value::as_str);
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(content) = delta.get("content").and_then(Value::as_str)
                && !content.is_empty()
            {
                events.push(ModelStreamEvent::TextDelta {
                    output_index,
                    content_index: None,
                    delta: content.to_string(),
                });
            }
            if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str)
                && !reasoning.is_empty()
            {
                events.push(ModelStreamEvent::ReasoningDelta {
                    output_index,
                    content_index: None,
                    delta: reasoning.to_string(),
                });
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for tool_call in tool_calls {
                    let index = tool_call
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or(output_index as u64) as usize;
                    let function = tool_call.get("function").unwrap_or(&Value::Null);
                    let call_id = tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let name = function
                        .get("name")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    if call_id.is_some() || name.is_some() {
                        events.push(ModelStreamEvent::ToolCallStarted {
                            output_index: index,
                            call_id: call_id.clone(),
                            name: name.clone(),
                        });
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str)
                        && !arguments.is_empty()
                    {
                        events.push(ModelStreamEvent::ToolCallArgumentsDelta {
                            output_index: index,
                            delta: arguments.to_string(),
                        });
                        if finish_reason == Some("tool_calls") {
                            tool_calls_completed.push((
                                index,
                                call_id.clone(),
                                name.clone(),
                                arguments.to_string(),
                            ));
                        }
                    }
                }
            }
        }
    }
    for (output_index, call_id, name, raw_arguments) in tool_calls_completed {
        let arguments = serde_json::from_str(&raw_arguments)
            .unwrap_or_else(|_| serde_json::json!({ "raw": raw_arguments.clone() }));
        events.push(ModelStreamEvent::OutputItemDone {
            output_index,
            item: ModelOutputItem::FunctionCall {
                call_id: call_id.unwrap_or_default(),
                name: name.unwrap_or_default(),
                arguments,
                raw_arguments,
            },
        });
    }
    let usage = match usage_parser {
        ChatUsageParser::OpenAiCompatible => parse_chat_usage(value.get("usage")),
        ChatUsageParser::Deepseek => parse_deepseek_chat_usage(value.get("usage")),
    };
    if usage.is_some() {
        events.push(ModelStreamEvent::Completed {
            id,
            usage,
            end_turn: None,
        });
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
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
            wire_api: ModelWireApi::ChatCompletions,
            capabilities: Default::default(),
            request_policy: Default::default(),
        }
    }

    fn deepseek_model() -> mai_protocol::ModelConfig {
        model_with_reasoning("deepseek-v4-pro", &["high", "max"], "high", |id| {
            serde_json::json!({
                "thinking": {
                    "type": "enabled",
                },
                "reasoning_effort": id,
            })
        })
    }

    #[test]
    fn chat_messages_fold_reasoning_and_function_call_into_assistant_message() {
        let messages = chat_messages(
            "instructions",
            &[
                ModelInputItem::user_text("hello"),
                ModelInputItem::Reasoning {
                    content: "thinking".to_string(),
                },
                ModelInputItem::FunctionCall {
                    call_id: "call_1".to_string(),
                    name: "container_exec".to_string(),
                    arguments: "{\"command\":\"pwd\"}".to_string(),
                },
            ],
        );

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(messages[2].reasoning_content.as_deref(), Some("thinking"));
        assert_eq!(messages[2].tool_calls[0].id, "call_1");
    }

    #[test]
    fn chat_messages_fold_consecutive_function_calls_into_one_assistant_message() {
        let messages = chat_messages(
            "instructions",
            &[
                ModelInputItem::user_text("hello"),
                ModelInputItem::Reasoning {
                    content: "thinking".to_string(),
                },
                ModelInputItem::FunctionCall {
                    call_id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: "{\"path\":\"Cargo.toml\"}".to_string(),
                },
                ModelInputItem::FunctionCall {
                    call_id: "call_2".to_string(),
                    name: "list_files".to_string(),
                    arguments: "{}".to_string(),
                },
                ModelInputItem::FunctionCallOutput {
                    call_id: "call_1".to_string(),
                    output: "done".to_string(),
                },
            ],
        );

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(messages[2].reasoning_content.as_deref(), Some("thinking"));
        assert_eq!(messages[2].tool_calls.len(), 2);
        assert_eq!(messages[2].tool_calls[0].id, "call_1");
        assert_eq!(messages[2].tool_calls[1].id, "call_2");
        assert_eq!(messages[3].role, "tool");
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn chat_messages_keep_reasoning_with_tool_calls_after_assistant_text_item() {
        let messages = chat_messages(
            "instructions",
            &[
                ModelInputItem::user_text("hello"),
                ModelInputItem::Reasoning {
                    content: "need repository facts".to_string(),
                },
                ModelInputItem::Message {
                    role: "assistant".to_string(),
                    content: vec![ModelContentItem::OutputText {
                        text: "I will inspect the repo.".to_string(),
                    }],
                },
                ModelInputItem::FunctionCall {
                    call_id: "call_1".to_string(),
                    name: "container_exec".to_string(),
                    arguments: "{\"command\":\"find . -maxdepth 2 -type f\"}".to_string(),
                },
                ModelInputItem::FunctionCallOutput {
                    call_id: "call_1".to_string(),
                    output: "Cargo.toml".to_string(),
                },
            ],
        );

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(
            messages[2].content.as_deref(),
            Some("I will inspect the repo.")
        );
        assert_eq!(
            messages[2].reasoning_content.as_deref(),
            Some("need repository facts")
        );
        assert_eq!(messages[2].tool_calls.len(), 1);
        assert_eq!(messages[3].role, "tool");
    }

    #[test]
    fn chat_messages_flush_reasoning_without_tool_call_as_assistant_message() {
        let messages = chat_messages(
            "instructions",
            &[
                ModelInputItem::user_text("first"),
                ModelInputItem::Reasoning {
                    content: "old thinking".to_string(),
                },
                ModelInputItem::user_text("second"),
            ],
        );

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(
            messages[2].reasoning_content.as_deref(),
            Some("old thinking")
        );
    }

    #[test]
    fn deepseek_request_uses_current_thinking_param_and_clamps_max_tokens() {
        let model = deepseek_model();
        let api = ChatCompletionsApi::deepseek();
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
                prompt_cache_key: Some("agent:agent-1:session:session-1"),
                max_output_tokens: 64_000,
                max_tokens_field: &model.request_policy.max_tokens_field,
                extra_body: crate::provider::request_options(&model, Some("high")),
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
        assert!(value.get("prompt_cache_key").is_none());
    }

    #[test]
    fn deepseek_reasoning_tool_call_messages_have_content_and_effort() {
        let model = deepseek_model();
        let api = ChatCompletionsApi::deepseek();
        let body = api
            .build_body(&WireRequest {
                model_id: &model.id,
                instructions: "instructions",
                input: &[
                    ModelInputItem::user_text("continue"),
                    ModelInputItem::Reasoning {
                        content: "need a tool".to_string(),
                    },
                    ModelInputItem::FunctionCall {
                        call_id: "call_1".to_string(),
                        name: "container_exec".to_string(),
                        arguments: "{\"command\":\"pwd\"}".to_string(),
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
                    parameters: serde_json::json!({ "type": "object" }),
                }],
                tool_choice: Some("auto"),
                stream: false,
                store: None,
                previous_response_id: None,
                prompt_cache_key: None,
                max_output_tokens: 64_000,
                max_tokens_field: &model.request_policy.max_tokens_field,
                extra_body: crate::provider::request_options(&model, Some("max")),
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
        let messages = value
            .get("messages")
            .and_then(Value::as_array)
            .expect("messages");
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
    fn chat_stream_emits_function_call_output_item_done_on_tool_finish() {
        let events = parse_chat_stream_chunk(serde_json::json!({
            "id": "chatcmpl_123",
            "choices": [{
                "index": 0,
                "finish_reason": "tool_calls",
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_123",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"src/main.rs\"}"
                        }
                    }]
                }
            }]
        }));

        assert!(events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::OutputItemDone {
                item: ModelOutputItem::FunctionCall { call_id, name, .. },
                ..
            } if call_id == "call_123" && name == "read_file"
        )));
    }

    #[test]
    fn mimo_request_policy_is_independent_from_deepseek_replay() {
        let mut model = model_with_reasoning(
            "gpt-5.5",
            &["minimal", "low", "medium", "high", "xhigh"],
            "medium",
            |id| {
                serde_json::json!({
                    "reasoning": {
                        "effort": id,
                    },
                })
            },
        );
        model.id = "mimo-v2.5-pro".to_string();
        model.wire_api = ModelWireApi::ChatCompletions;
        model.capabilities.reasoning_replay = false;
        model.request_policy.extra_body = serde_json::json!({ "mimo_only": true });

        let api = ChatCompletionsApi::openai_compatible();
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
                prompt_cache_key: Some("agent:agent-1:session:session-1"),
                max_output_tokens: 131_072,
                max_tokens_field: &model.request_policy.max_tokens_field,
                extra_body: crate::provider::request_options(&model, None),
                supports_tools: true,
            })
            .expect("build");
        let value: Value = serde_json::from_slice(&body).expect("parse");

        assert_eq!(value.get("mimo_only").and_then(Value::as_bool), Some(true));
        assert!(value.get("thinking").is_none());
        assert!(value.get("prompt_cache_key").is_none());
    }

    #[test]
    fn chat_request_uses_configured_max_token_field() {
        let mut model = deepseek_model();
        model.request_policy.max_tokens_field = "max_completion_tokens".to_string();
        let api = ChatCompletionsApi::deepseek();
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
                prompt_cache_key: None,
                max_output_tokens: 64_000,
                max_tokens_field: &model.request_policy.max_tokens_field,
                extra_body: crate::provider::request_options(&model, Some("high")),
                supports_tools: true,
            })
            .expect("build");
        let value: Value = serde_json::from_slice(&body).expect("parse");

        assert_eq!(value["max_completion_tokens"].as_u64(), Some(64_000));
        assert!(value.get("max_tokens").is_none());
    }

    #[test]
    fn chat_chunk_with_null_usage_does_not_complete_stream() {
        let events = parse_chat_stream_chunk(serde_json::json!({
            "id": "chunk_1",
            "object": "chat.completion.chunk",
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "content": "",
                    "reasoning_content": null,
                    "tool_calls": null
                },
                "finish_reason": null
            }],
            "usage": null
        }));

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ModelStreamEvent::ResponseStarted { id } if id.as_deref() == Some("chunk_1")
        ));
    }
}
