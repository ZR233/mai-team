use crate::error::{ModelError, Result};
use crate::provider::{ProviderResolver, ResolvedProvider, response_id_unsupported_for_responses_http};
use crate::types::ModelTurnState;
use crate::wire::responses::openai_turn_input;
use crate::wire::WireRequest;
use mai_protocol::{ModelInputItem, ModelResponse, ToolDefinition};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ModelClient {
    http: reqwest::Client,
    resolver: Arc<dyn ProviderResolver>,
    continuation_cache: Arc<Mutex<HashSet<String>>>,
}

impl ModelClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resolve(
        &self,
        provider: &mai_protocol::ProviderSecret,
        model: &mai_protocol::ModelConfig,
        reasoning_effort: Option<&str>,
    ) -> ResolvedProvider {
        self.resolver.resolve(provider, model, reasoning_effort)
    }

    pub async fn send(
        &self,
        resolved: &ResolvedProvider,
        instructions: &str,
        input: &[ModelInputItem],
        tools: &[ToolDefinition],
    ) -> Result<ModelResponse> {
        let wire_req = WireRequest {
            model_id: &resolved.model_id,
            instructions,
            input,
            tools,
            tool_choice: tool_choice(tools, resolved.supports_tools),
            stream: false,
            store: Some(false),
            previous_response_id: None,
            max_output_tokens: resolved.max_output_tokens,
            extra_body: resolved.extra_body.clone(),
            supports_tools: resolved.supports_tools,
        };
        self.send_request(resolved, &wire_req).await
    }

    pub async fn send_with_cancel(
        &self,
        resolved: &ResolvedProvider,
        instructions: &str,
        input: &[ModelInputItem],
        tools: &[ToolDefinition],
        cancellation_token: &CancellationToken,
    ) -> Result<ModelResponse> {
        tokio::select! {
            response = self.send(resolved, instructions, input, tools) => response,
            _ = cancellation_token.cancelled() => Err(ModelError::Cancelled),
        }
    }

    pub async fn send_turn(
        &self,
        resolved: &ResolvedProvider,
        instructions: &str,
        input: &[ModelInputItem],
        tools: &[ToolDefinition],
        state: &mut ModelTurnState,
        cancellation_token: &CancellationToken,
    ) -> Result<ModelResponse> {
        tokio::select! {
            response = self.send_turn_inner(resolved, instructions, input, tools, state) => response,
            _ = cancellation_token.cancelled() => Err(ModelError::Cancelled),
        }
    }

    async fn send_turn_inner(
        &self,
        resolved: &ResolvedProvider,
        instructions: &str,
        input: &[ModelInputItem],
        tools: &[ToolDefinition],
        state: &mut ModelTurnState,
    ) -> Result<ModelResponse> {
        if self.continuation_is_unsupported(&resolved.cache_key).await {
            state.continuation_disabled = true;
            state.previous_response_id = None;
        }
        if !resolved.supports_continuation || state.continuation_disabled {
            return self.send(resolved, instructions, input, tools).await;
        }

        let (delta_input, previous_response_id) = openai_turn_input(input, state);
        let wire_req = WireRequest {
            model_id: &resolved.model_id,
            instructions,
            input: delta_input,
            tools,
            tool_choice: tool_choice(tools, resolved.supports_tools),
            stream: false,
            store: Some(true),
            previous_response_id,
            max_output_tokens: resolved.max_output_tokens,
            extra_body: resolved.extra_body.clone(),
            supports_tools: resolved.supports_tools,
        };
        let response = self.send_request(resolved, &wire_req).await;
        let response = match response {
            Ok(response) => response,
            Err(err)
                if previous_response_id.is_some()
                    && response_id_unsupported_for_responses_http(&err) =>
            {
                self.mark_continuation_unsupported(&resolved.cache_key).await;
                state.continuation_disabled = true;
                state.previous_response_id = None;
                self.send(resolved, instructions, input, tools).await?
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

    async fn send_request(
        &self,
        resolved: &ResolvedProvider,
        wire_req: &WireRequest<'_>,
    ) -> Result<ModelResponse> {
        let body = resolved.wire_protocol.build_body(wire_req)?;
        let response = self
            .http
            .post(&resolved.endpoint)
            .bearer_auth(&resolved.api_key)
            .headers(resolved.headers.clone())
            .body(body)
            .send()
            .await
            .map_err(|source| ModelError::Request {
                endpoint: resolved.endpoint.clone(),
                source,
            })?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|source| ModelError::Request {
                endpoint: resolved.endpoint.clone(),
                source,
            })?;
        if !status.is_success() {
            return Err(ModelError::Api {
                endpoint: resolved.endpoint.clone(),
                status,
                body: text,
            });
        }
        resolved.wire_protocol.parse_response(&text)
    }

    async fn continuation_is_unsupported(&self, cache_key: &str) -> bool {
        self.continuation_cache
            .lock()
            .await
            .contains(cache_key)
    }

    async fn mark_continuation_unsupported(&self, cache_key: &str) {
        self.continuation_cache
            .lock()
            .await
            .insert(cache_key.to_string());
    }
}

fn tool_choice(tools: &[ToolDefinition], supports_tools: bool) -> Option<&'static str> {
    (!tools.is_empty() && supports_tools).then_some("auto")
}

impl Default for ModelClient {
    fn default() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            resolver: Arc::new(crate::provider::DefaultProviderResolver::new()),
            continuation_cache: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{
        ModelConfig, ModelReasoningConfig, ModelReasoningVariant,
        ModelWireApi, ProviderKind, ProviderSecret,
    };
    use serde_json::{Value, json};
    use std::collections::VecDeque;
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
            headers: std::collections::BTreeMap::new(),
            wire_api: ModelWireApi::Responses,
            capabilities: Default::default(),
            request_policy: Default::default(),
        }
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

    async fn start_mock_responses(
        responses: Vec<Value>,
    ) -> (String, Arc<Mutex<Vec<Value>>>) {
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
        let client = ModelClient::new();
        let cancellation_token = CancellationToken::new();
        let mut state = ModelTurnState::default();

        let first_input = vec![ModelInputItem::user_text("first")];
        let resolved = client.resolve(&provider, &model, None);
        let first = client
            .send_turn(&resolved, "instructions", &first_input, &[], &mut state, &cancellation_token)
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
            .send_turn(&resolved, "instructions", &second_input, &[], &mut state, &cancellation_token)
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
        let client = ModelClient::new();
        let cancellation_token = CancellationToken::new();
        let resolved = client.resolve(&provider, &model, None);

        let mut first_state = ModelTurnState::default();
        let first_input = vec![ModelInputItem::user_text("first")];
        client
            .send_turn(&resolved, "instructions", &first_input, &[], &mut first_state, &cancellation_token)
            .await
            .expect("first response");
        first_state.acknowledge_history_len(1);
        let second_input = vec![
            ModelInputItem::user_text("first"),
            ModelInputItem::assistant_text("ok"),
            ModelInputItem::user_text("second"),
        ];
        client
            .send_turn(&resolved, "instructions", &second_input, &[], &mut first_state, &cancellation_token)
            .await
            .expect("fallback response");

        let mut next_state = ModelTurnState {
            previous_response_id: Some("resp_cached".to_string()),
            acknowledged_input_len: 1,
            ..Default::default()
        };
        client
            .send_turn(&resolved, "instructions", &second_input, &[], &mut next_state, &cancellation_token)
            .await
            .expect("cached no-continuation response");

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[1]["previous_response_id"], "resp_1");
        assert!(requests[2].get("previous_response_id").is_none());
        assert!(requests[3].get("previous_response_id").is_none());
        assert_eq!(requests[3]["store"], false);
    }
}
