use mai_protocol::{
    ModelConfig, ModelInputItem, ModelOutputItem, ModelOutputToolCall, ModelResponse, ModelWireApi,
    ProviderKind, ProviderSecret, TokenUsage, ToolDefinition,
};
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

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
    #[error("request cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, ModelError>;

#[derive(Debug, Clone)]
pub struct ResponsesClient {
    http: reqwest::Client,
    unsupported_http_continuation: Arc<Mutex<HashSet<String>>>,
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
    store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<&'a str>,
    #[serde(flatten)]
    options: BTreeMap<String, Value>,
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

pub struct ModelRequest<'a> {
    pub provider: &'a ProviderSecret,
    pub model: &'a ModelConfig,
    pub instructions: &'a str,
    pub input: &'a [ModelInputItem],
    pub tools: &'a [ToolDefinition],
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ModelTurnState {
    pub previous_response_id: Option<String>,
    pub acknowledged_input_len: usize,
    continuation_disabled: bool,
}

impl ModelTurnState {
    pub fn acknowledge_history_len(&mut self, len: usize) {
        self.acknowledged_input_len = len;
    }
}

struct ResponseOptions<'a> {
    reasoning_effort: Option<String>,
    previous_response_id: Option<&'a str>,
    store: Option<bool>,
}

impl ResponsesClient {
    pub fn new() -> Self {
        Self::default()
    }

    async fn http_continuation_is_unsupported(
        &self,
        provider: &ProviderSecret,
        model: &ModelConfig,
    ) -> bool {
        self.unsupported_http_continuation
            .lock()
            .await
            .contains(&continuation_cache_key(provider, model))
    }

    async fn mark_http_continuation_unsupported(
        &self,
        provider: &ProviderSecret,
        model: &ModelConfig,
    ) {
        self.unsupported_http_continuation
            .lock()
            .await
            .insert(continuation_cache_key(provider, model));
    }

    pub async fn create_response(
        &self,
        provider: &ProviderSecret,
        model: &ModelConfig,
        instructions: &str,
        input: &[ModelInputItem],
        tools: &[ToolDefinition],
        reasoning_effort: Option<String>,
    ) -> Result<ModelResponse> {
        match model_wire_api(provider, model) {
            ModelWireApi::Responses => {
                self.create_openai_response(
                    provider,
                    model,
                    instructions,
                    input,
                    tools,
                    ResponseOptions {
                        reasoning_effort,
                        previous_response_id: None,
                        store: Some(false),
                    },
                )
                .await
            }
            ModelWireApi::ChatCompletions if provider.kind == ProviderKind::Deepseek => {
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
            ModelWireApi::ChatCompletions => {
                self.create_mimo_chat(
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

    pub async fn create_response_with_cancel(
        &self,
        req: &ModelRequest<'_>,
        cancellation_token: &CancellationToken,
    ) -> Result<ModelResponse> {
        tokio::select! {
            response = self.create_response(
                req.provider,
                req.model,
                req.instructions,
                req.input,
                req.tools,
                req.reasoning_effort.clone(),
            ) => response,
            _ = cancellation_token.cancelled() => Err(ModelError::Cancelled),
        }
    }

    pub async fn create_turn_response_with_cancel(
        &self,
        req: &ModelRequest<'_>,
        state: &mut ModelTurnState,
        cancellation_token: &CancellationToken,
    ) -> Result<ModelResponse> {
        tokio::select! {
            response = self.create_turn_response(req, state) => response,
            _ = cancellation_token.cancelled() => Err(ModelError::Cancelled),
        }
    }

    async fn create_turn_response(
        &self,
        req: &ModelRequest<'_>,
        state: &mut ModelTurnState,
    ) -> Result<ModelResponse> {
        if self
            .http_continuation_is_unsupported(req.provider, req.model)
            .await
        {
            state.continuation_disabled = true;
            state.previous_response_id = None;
        }
        if model_wire_api(req.provider, req.model) != ModelWireApi::Responses
            || !req.model.capabilities.continuation
            || state.continuation_disabled
        {
            return self
                .create_response(
                    req.provider,
                    req.model,
                    req.instructions,
                    req.input,
                    req.tools,
                    req.reasoning_effort.clone(),
                )
                .await;
        }

        let (input, previous_response_id) = openai_turn_input(req.input, state);
        let response = self
            .create_openai_response(
                req.provider,
                req.model,
                req.instructions,
                input,
                req.tools,
                ResponseOptions {
                    reasoning_effort: req.reasoning_effort.clone(),
                    previous_response_id,
                    store: Some(true),
                },
            )
            .await;
        let response = match response {
            Ok(response) => response,
            Err(err)
                if previous_response_id.is_some()
                    && response_id_unsupported_for_responses_http(&err) =>
            {
                self.mark_http_continuation_unsupported(req.provider, req.model)
                    .await;
                state.continuation_disabled = true;
                state.previous_response_id = None;
                self.create_response(
                    req.provider,
                    req.model,
                    req.instructions,
                    req.input,
                    req.tools,
                    req.reasoning_effort.clone(),
                )
                .await?
            }
            Err(err) => return Err(err),
        };
        if let Some(id) = &response.id {
            if state.continuation_disabled {
                state.previous_response_id = None;
            } else {
                state.previous_response_id = Some(id.clone());
            }
        }
        Ok(response)
    }

    async fn create_openai_response(
        &self,
        provider: &ProviderSecret,
        model: &ModelConfig,
        instructions: &str,
        input: &[ModelInputItem],
        tools: &[ToolDefinition],
        options: ResponseOptions<'_>,
    ) -> Result<ModelResponse> {
        let endpoint = format!("{}/responses", provider.base_url.trim_end_matches('/'));
        let active_tools = if model_supports_tools(model) {
            tools
        } else {
            &[]
        };
        let request = ResponsesRequest {
            model: &model.id,
            instructions,
            input,
            tools: active_tools,
            tool_choice: (!active_tools.is_empty()).then_some("auto"),
            stream: false,
            store: options.store.or(model.request_policy.store),
            previous_response_id: options.previous_response_id,
            options: request_options(model, options.reasoning_effort.as_deref()),
        };
        let response = self
            .http
            .post(&endpoint)
            .bearer_auth(&provider.api_key)
            .headers(headers(&provider.headers(&request_headers(model))))
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
        reasoning_effort: Option<String>,
    ) -> Result<ModelResponse> {
        let endpoint = format!(
            "{}/chat/completions",
            provider.base_url.trim_end_matches('/')
        );
        let active_tools = if model_supports_tools(model) {
            tools.iter().map(chat_tool).collect()
        } else {
            Vec::new()
        };
        let request = deepseek_chat_request(
            model,
            instructions,
            input,
            active_tools,
            reasoning_effort.as_deref(),
        );
        let response = self
            .http
            .post(&endpoint)
            .bearer_auth(&provider.api_key)
            .headers(headers(&provider.headers(&request_headers(model))))
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

    async fn create_mimo_chat(
        &self,
        provider: &ProviderSecret,
        model: &ModelConfig,
        instructions: &str,
        input: &[ModelInputItem],
        tools: &[ToolDefinition],
        reasoning_effort: Option<String>,
    ) -> Result<ModelResponse> {
        let endpoint = format!(
            "{}/chat/completions",
            provider.base_url.trim_end_matches('/')
        );
        let active_tools = if model_supports_tools(model) {
            tools.iter().map(chat_tool).collect()
        } else {
            Vec::new()
        };
        let request = mimo_chat_request(
            model,
            instructions,
            input,
            active_tools,
            reasoning_effort.as_deref(),
        );
        let response = self
            .http
            .post(&endpoint)
            .bearer_auth(&provider.api_key)
            .headers(headers(&provider.headers(&request_headers(model))))
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
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            unsupported_http_continuation: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

fn model_wire_api(provider: &ProviderSecret, model: &ModelConfig) -> ModelWireApi {
    match provider.kind {
        ProviderKind::Openai => model.wire_api,
        ProviderKind::Deepseek | ProviderKind::Mimo => ModelWireApi::ChatCompletions,
    }
}

fn model_supports_tools(model: &ModelConfig) -> bool {
    model.supports_tools && model.capabilities.tools
}

fn response_id_unsupported_for_responses_http(err: &ModelError) -> bool {
    let ModelError::Api { status, body, .. } = err else {
        return false;
    };
    *status == StatusCode::BAD_REQUEST
        && body.contains("previous_response_id")
        && body.contains("only supported on Responses WebSocket")
}

fn continuation_cache_key(provider: &ProviderSecret, model: &ModelConfig) -> String {
    format!(
        "{:?}|{}|{}",
        provider.kind,
        provider.base_url.trim_end_matches('/'),
        model.id
    )
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

fn request_headers(model: &ModelConfig) -> BTreeMap<String, String> {
    let mut headers = model.headers.clone();
    headers.extend(model.request_policy.headers.clone());
    headers
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

fn request_options(model: &ModelConfig, reasoning_effort: Option<&str>) -> BTreeMap<String, Value> {
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

fn deepseek_max_tokens(configured: u64) -> u64 {
    configured.clamp(1, 64_000)
}

fn mimo_max_tokens(configured: u64) -> u64 {
    configured.clamp(1, 131_072)
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
    reasoning_effort: Option<&str>,
) -> ChatRequest {
    ChatRequest {
        model: model.id.clone(),
        messages: chat_messages(instructions, input),
        tool_choice: (!tools.is_empty()).then_some("auto"),
        tools,
        stream: false,
        max_tokens: deepseek_max_tokens(model.output_tokens),
        options: request_options(model, reasoning_effort),
    }
}

fn mimo_chat_request(
    model: &ModelConfig,
    instructions: &str,
    input: &[ModelInputItem],
    tools: Vec<ChatTool>,
    reasoning_effort: Option<&str>,
) -> ChatRequest {
    ChatRequest {
        model: model.id.clone(),
        messages: chat_messages(instructions, input),
        tool_choice: (!tools.is_empty()).then_some("auto"),
        tools,
        stream: false,
        max_tokens: mimo_max_tokens(model.output_tokens),
        options: request_options(model, reasoning_effort),
    }
}

fn openai_turn_input<'a>(
    input: &'a [ModelInputItem],
    state: &'a ModelTurnState,
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

#[cfg(test)]
fn openai_turn_input_for_test<'a>(
    input: &'a [ModelInputItem],
    state: &'a ModelTurnState,
) -> (&'a [ModelInputItem], Option<&'a str>) {
    openai_turn_input(input, state)
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
                    content: assistant_chat_content(
                        content,
                        &tool_calls,
                        reasoning_content.as_deref(),
                    ),
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
    use std::collections::VecDeque;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Mutex;

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
            reasoning: Some(mai_protocol::ModelReasoningConfig {
                default_variant: Some(default_variant.to_string()),
                variants: variants
                    .iter()
                    .map(|id| mai_protocol::ModelReasoningVariant {
                        id: (*id).to_string(),
                        label: None,
                        request: request_for(id),
                    })
                    .collect(),
            }),
            options: serde_json::Value::Null,
            headers: BTreeMap::new(),
            wire_api: ModelWireApi::Responses,
            capabilities: Default::default(),
            request_policy: Default::default(),
        }
    }

    fn deepseek_model() -> ModelConfig {
        model_with_reasoning("deepseek-v4-pro", &["high", "max"], "high", |id| {
            json!({
                "thinking": {
                    "type": "enabled",
                },
                "reasoning_effort": id,
            })
        })
    }

    fn openai_model() -> ModelConfig {
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
        let model = deepseek_model();
        let request = deepseek_chat_request(
            &model,
            "instructions",
            &[ModelInputItem::user_text("hello")],
            Vec::new(),
            Some("high"),
        );
        let value = serde_json::to_value(&request).expect("request json");

        assert_eq!(request.max_tokens, 64_000);
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
        let request = deepseek_chat_request(
            &model,
            "instructions",
            &[
                ModelInputItem::user_text("continue"),
                ModelInputItem::AssistantTurn {
                    content: None,
                    reasoning_content: Some("need a tool".to_string()),
                    tool_calls: vec![mai_protocol::ModelToolCall {
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
            vec![ChatTool {
                kind: "function",
                function: ChatToolFunction {
                    name: "container_exec".to_string(),
                    description: "run a command".to_string(),
                    parameters: json!({ "type": "object" }),
                },
            }],
            Some("max"),
        );
        let value = serde_json::to_value(&request).expect("request json");

        assert_eq!(
            value.get("reasoning_effort").and_then(Value::as_str),
            Some("max")
        );
        assert_eq!(
            value.pointer("/thinking/type").and_then(Value::as_str),
            Some("enabled")
        );
        assert_eq!(request.messages[2].role, "assistant");
        assert_eq!(request.messages[2].content.as_deref(), Some(""));
        assert_eq!(
            request.messages[2].reasoning_content.as_deref(),
            Some("need a tool")
        );
        assert_eq!(request.messages[2].tool_calls.len(), 1);
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
        let (request_input, previous_response_id) = openai_turn_input_for_test(&input, &state);
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

    #[tokio::test]
    async fn openai_turn_falls_back_when_http_responses_reject_previous_response_id() {
        let (base_url, requests) = start_mock_responses(vec![
            json!({
                "id": "resp_1",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "ok" }]
                    }
                ],
                "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
            }),
            json!({
                "__status": 400,
                "error": {
                    "message": "previous_response_id is only supported on Responses WebSocket v2",
                    "type": "invalid_request_error"
                }
            }),
            json!({
                "id": "resp_2",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "recovered" }]
                    }
                ],
                "usage": { "input_tokens": 3, "output_tokens": 1, "total_tokens": 4 }
            }),
        ])
        .await;
        let provider = openai_provider_secret(base_url);
        let model = openai_model();
        let client = ResponsesClient::new();
        let cancellation_token = CancellationToken::new();
        let mut state = ModelTurnState::default();

        let first_input = vec![ModelInputItem::user_text("first")];
        let first = client
            .create_turn_response_with_cancel(
                &ModelRequest {
                    provider: &provider,
                    model: &model,
                    instructions: "instructions",
                    input: &first_input,
                    tools: &[],
                    reasoning_effort: None,
                },
                &mut state,
                &cancellation_token,
            )
            .await
            .expect("first response");
        assert_eq!(first.id.as_deref(), Some("resp_1"));
        assert_eq!(state.previous_response_id.as_deref(), Some("resp_1"));

        let second_input = vec![
            ModelInputItem::user_text("first"),
            ModelInputItem::assistant_text("ok"),
            ModelInputItem::user_text("second"),
        ];
        state.acknowledge_history_len(2);
        let second = client
            .create_turn_response_with_cancel(
                &ModelRequest {
                    provider: &provider,
                    model: &model,
                    instructions: "instructions",
                    input: &second_input,
                    tools: &[],
                    reasoning_effort: None,
                },
                &mut state,
                &cancellation_token,
            )
            .await
            .expect("fallback response");
        assert_eq!(second.id.as_deref(), Some("resp_2"));
        assert_eq!(state.previous_response_id, None);
        assert!(state.continuation_disabled);

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert!(requests[0].get("previous_response_id").is_none());
        assert_eq!(requests[0]["store"], true);
        assert_eq!(requests[1]["previous_response_id"], "resp_1");
        assert_eq!(
            requests[1]["input"].as_array().expect("delta input").len(),
            1
        );
        assert_eq!(requests[1]["input"][0]["role"], "user");
        assert!(requests[2].get("previous_response_id").is_none());
        assert_eq!(requests[2]["store"], false);
        assert_eq!(
            requests[2]["input"].as_array().expect("full input").len(),
            3
        );
    }

    #[tokio::test]
    async fn openai_http_continuation_unsupported_is_cached_across_turns() {
        let (base_url, requests) = start_mock_responses(vec![
            json!({
                "id": "resp_1",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "ok" }]
                    }
                ]
            }),
            json!({
                "__status": 400,
                "error": {
                    "message": "previous_response_id is only supported on Responses WebSocket v2",
                    "type": "invalid_request_error"
                }
            }),
            json!({
                "id": "resp_2",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "fallback" }]
                    }
                ]
            }),
            json!({
                "id": "resp_3",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "next" }]
                    }
                ]
            }),
        ])
        .await;
        let provider = openai_provider_secret(base_url);
        let model = openai_model();
        let client = ResponsesClient::new();
        let cancellation_token = CancellationToken::new();
        let mut first_state = ModelTurnState::default();

        let first_input = vec![ModelInputItem::user_text("first")];
        client
            .create_turn_response_with_cancel(
                &ModelRequest {
                    provider: &provider,
                    model: &model,
                    instructions: "instructions",
                    input: &first_input,
                    tools: &[],
                    reasoning_effort: None,
                },
                &mut first_state,
                &cancellation_token,
            )
            .await
            .expect("first response");
        first_state.acknowledge_history_len(1);
        let second_input = vec![
            ModelInputItem::user_text("first"),
            ModelInputItem::assistant_text("ok"),
            ModelInputItem::user_text("second"),
        ];
        client
            .create_turn_response_with_cancel(
                &ModelRequest {
                    provider: &provider,
                    model: &model,
                    instructions: "instructions",
                    input: &second_input,
                    tools: &[],
                    reasoning_effort: None,
                },
                &mut first_state,
                &cancellation_token,
            )
            .await
            .expect("fallback response");

        let mut next_state = ModelTurnState {
            previous_response_id: Some("resp_cached".to_string()),
            acknowledged_input_len: 1,
            ..Default::default()
        };
        client
            .create_turn_response_with_cancel(
                &ModelRequest {
                    provider: &provider,
                    model: &model,
                    instructions: "instructions",
                    input: &second_input,
                    tools: &[],
                    reasoning_effort: None,
                },
                &mut next_state,
                &cancellation_token,
            )
            .await
            .expect("cached no-continuation response");

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[1]["previous_response_id"], "resp_1");
        assert!(requests[2].get("previous_response_id").is_none());
        assert!(requests[3].get("previous_response_id").is_none());
        assert_eq!(requests[3]["store"], false);
    }

    #[test]
    fn mimo_request_policy_is_independent_from_deepseek_replay() {
        let mut model = openai_model();
        model.id = "mimo-v2.5-pro".to_string();
        model.wire_api = ModelWireApi::ChatCompletions;
        model.capabilities.reasoning_replay = false;
        model.request_policy.extra_body = json!({ "mimo_only": true });
        let request = mimo_chat_request(
            &model,
            "instructions",
            &[ModelInputItem::user_text("hello")],
            Vec::new(),
            None,
        );
        let value = serde_json::to_value(&request).expect("request json");

        assert_eq!(value.get("mimo_only").and_then(Value::as_bool), Some(true));
        assert!(value.get("thinking").is_none());
    }

    #[test]
    fn reasoning_variant_request_deep_merges_over_model_options() {
        let mut model = openai_model();
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

    fn openai_provider_secret(base_url: String) -> ProviderSecret {
        ProviderSecret {
            id: "openai".to_string(),
            kind: ProviderKind::Openai,
            name: "OpenAI".to_string(),
            base_url,
            api_key: "secret".to_string(),
            api_key_env: None,
            models: Vec::new(),
            default_model: "gpt-5.5".to_string(),
            enabled: true,
        }
    }

    async fn start_mock_responses(responses: Vec<Value>) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock server addr");
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_responses = Arc::clone(&responses);
        let server_requests = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let responses = Arc::clone(&server_responses);
                let requests = Arc::clone(&server_requests);
                tokio::spawn(async move {
                    let request = read_mock_request(&mut stream).await;
                    requests.lock().await.push(request);
                    let response = responses.lock().await.pop_front().unwrap_or_else(|| {
                        json!({
                            "id": "resp_empty",
                            "output": [],
                            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
                        })
                    });
                    write_mock_response(&mut stream, response).await;
                });
            }
        });
        (format!("http://{addr}"), requests)
    }

    async fn read_mock_request(stream: &mut tokio::net::TcpStream) -> Value {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk).await.expect("read request");
            assert!(read > 0, "mock request closed before headers");
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(header_end) = find_header_end(&buffer) {
                break header_end;
            }
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let content_length = content_length(&headers);
        while buffer.len() < header_end + content_length {
            let read = stream.read(&mut chunk).await.expect("read request body");
            assert!(read > 0, "mock request closed before body");
            buffer.extend_from_slice(&chunk[..read]);
        }
        serde_json::from_slice(&buffer[header_end..header_end + content_length])
            .expect("request json")
    }

    async fn write_mock_response(stream: &mut tokio::net::TcpStream, response: Value) {
        let status = response
            .get("__status")
            .and_then(Value::as_u64)
            .unwrap_or(200);
        let mut body_value = response;
        if let Some(object) = body_value.as_object_mut() {
            object.remove("__status");
        }
        let body = serde_json::to_string(&body_value).expect("response json");
        let reason = if status == 200 { "OK" } else { "ERROR" };
        let reply = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(reply.as_bytes())
            .await
            .expect("write response");
    }

    fn find_header_end(buffer: &[u8]) -> Option<usize> {
        buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
    }

    fn content_length(headers: &str) -> usize {
        headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0)
    }
}
