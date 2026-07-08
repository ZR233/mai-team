use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use axum::http::StatusCode;
use mai_protocol::*;
use mai_runtime::{
    completion_response_usage, core_model_turn_request, core_provider_for_selection,
    model_supports_continuation,
};
use mai_store::ConfigStore;
use pl_core::{CoreModelTurnOptions, CoreSession, completion_response_preview, user_text_message};
use pl_model::CompletionResponse;
use pl_protocol::PureError;
use tokio_util::sync::CancellationToken;

fn elapsed_millis(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn sanitize_provider_test_error(err: &PureError, api_key: &str) -> String {
    mai_protocol::preview(&redact_secret(&err.to_string(), api_key), 1_500)
}

fn redact_secret(value: &str, secret: &str) -> String {
    if secret.trim().is_empty() {
        value.to_string()
    } else {
        value.replace(secret, "[redacted]")
    }
}

pub(crate) struct ProviderService {
    store: Arc<ConfigStore>,
}

impl ProviderService {
    pub(crate) fn new(store: Arc<ConfigStore>) -> Self {
        Self { store }
    }

    pub(crate) async fn providers_response(
        &self,
    ) -> Result<ProvidersResponse, mai_store::StoreError> {
        self.store.providers_response().await
    }

    pub(crate) async fn save_providers(
        &self,
        request: ProvidersConfigRequest,
    ) -> Result<ProvidersResponse, mai_store::StoreError> {
        self.store.save_providers(request).await?;
        self.store.providers_response().await
    }

    pub(crate) async fn mcp_servers(
        &self,
    ) -> Result<McpServersConfigRequest, mai_store::StoreError> {
        Ok(McpServersConfigRequest {
            servers: self.store.list_mcp_servers().await?,
        })
    }

    pub(crate) async fn save_mcp_servers(
        &self,
        servers: &BTreeMap<String, McpServerConfig>,
    ) -> Result<McpServersConfigRequest, mai_store::StoreError> {
        self.store.save_mcp_servers(servers).await?;
        Ok(McpServersConfigRequest {
            servers: self.store.list_mcp_servers().await?,
        })
    }

    pub(crate) async fn test_provider(
        &self,
        provider_id: &str,
        request: ProviderTestRequest,
    ) -> ProviderTestResult {
        run_provider_test(&self.store, provider_id, request).await
    }
}

pub(crate) struct ProviderTestResult {
    pub(crate) status: StatusCode,
    pub(crate) response: ProviderTestResponse,
}

async fn run_provider_test(
    store: &ConfigStore,
    provider_id: &str,
    request: ProviderTestRequest,
) -> ProviderTestResult {
    let started = Instant::now();
    let selection = match store
        .resolve_provider(Some(provider_id), request.model.as_deref())
        .await
    {
        Ok(selection) => selection,
        Err(err) => {
            let provider = store.get_provider_secret(provider_id).await.ok().flatten();
            let model = request.model.clone().or_else(|| {
                provider
                    .as_ref()
                    .map(|provider| provider.default_model.clone())
            });
            return ProviderTestResult {
                status: StatusCode::BAD_REQUEST,
                response: ProviderTestResponse {
                    ok: false,
                    provider_id: provider
                        .as_ref()
                        .map(|provider| provider.id.clone())
                        .unwrap_or_else(|| provider_id.to_string()),
                    provider_name: provider
                        .as_ref()
                        .map(|provider| provider.name.clone())
                        .unwrap_or_default(),
                    provider_kind: provider
                        .as_ref()
                        .map(|provider| provider.kind)
                        .unwrap_or_default(),
                    model: model.unwrap_or_default(),
                    base_url: provider
                        .as_ref()
                        .map(|provider| provider.base_url.clone())
                        .unwrap_or_default(),
                    latency_ms: elapsed_millis(started),
                    output_preview: String::new(),
                    usage: None,
                    error: Some(err.to_string()),
                },
            };
        }
    };

    let provider = selection.provider.clone();
    let model = selection.model.clone();
    let base_url = provider.base_url.clone();
    let reasoning_effort = request.reasoning_effort;
    let tester = ProviderTester::new();
    let response = tester
        .run_test(&selection, reasoning_effort.as_deref(), request.deep)
        .await;
    let latency_ms = elapsed_millis(started);
    match response {
        Ok(response) => ProviderTestResult {
            status: StatusCode::OK,
            response: ProviderTestResponse {
                ok: true,
                provider_id: provider.id,
                provider_name: provider.name,
                provider_kind: provider.kind,
                model: model.id,
                base_url,
                latency_ms,
                output_preview: completion_response_preview(&response),
                usage: Some(completion_response_usage(&response.usage)),
                error: None,
            },
        },
        Err(err) => ProviderTestResult {
            status: StatusCode::OK,
            response: ProviderTestResponse {
                ok: false,
                provider_id: provider.id,
                provider_name: provider.name,
                provider_kind: provider.kind,
                model: model.id,
                base_url,
                latency_ms,
                output_preview: String::new(),
                usage: None,
                error: Some(sanitize_provider_test_error(&err, &provider.api_key)),
            },
        },
    }
}

pub(crate) struct ProviderTester;

impl ProviderTester {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) async fn run_test(
        &self,
        selection: &mai_store::ProviderSelection,
        reasoning_effort: Option<&str>,
        deep: bool,
    ) -> std::result::Result<CompletionResponse, PureError> {
        if deep && model_supports_continuation(selection) {
            self.run_deep_test(selection, reasoning_effort).await
        } else {
            self.run_single_test(selection, reasoning_effort).await
        }
    }

    async fn run_single_test(
        &self,
        selection: &mai_store::ProviderSelection,
        reasoning_effort: Option<&str>,
    ) -> std::result::Result<CompletionResponse, PureError> {
        let mut session = CoreSession::from_messages(vec![user_text_message("ping")]);
        let provider = core_provider_for_selection(selection)?;
        let request = core_model_turn_request(
            selection,
            reasoning_effort,
            "You are a provider connectivity test. Reply with exactly: ok",
            Vec::new(),
        );
        pl_core::stream_session_completion_response(
            provider,
            &mut session,
            request,
            CoreModelTurnOptions::default().with_cancellation(CancellationToken::new()),
        )
        .await
    }

    async fn run_deep_test(
        &self,
        selection: &mai_store::ProviderSelection,
        reasoning_effort: Option<&str>,
    ) -> std::result::Result<CompletionResponse, PureError> {
        let provider = core_provider_for_selection(selection)?;
        let mut session = CoreSession::from_messages(vec![user_text_message(
            "Provider deep connectivity test, step 1. Reply exactly: ok",
        )]);
        let instructions = "You are a provider connectivity test. Reply with exactly: ok";
        let first = pl_core::stream_session_completion_response(
            provider.clone(),
            &mut session,
            core_model_turn_request(selection, reasoning_effort, instructions, Vec::new()),
            CoreModelTurnOptions::default().with_cancellation(CancellationToken::new()),
        )
        .await?;
        session.push_assistant_response(completion_response_preview(&first), None);
        session.push_user_prompt(
            "Provider deep connectivity test, step 2. Reply exactly: ok".to_string(),
        );
        pl_core::stream_session_completion_response(
            provider,
            &mut session,
            core_model_turn_request(selection, reasoning_effort, instructions, Vec::new()),
            CoreModelTurnOptions::default().with_cancellation(CancellationToken::new()),
        )
        .await
    }
}

#[cfg(test)]
pub(crate) async fn provider_test_store(
    provider: ProviderConfig,
) -> (tempfile::TempDir, ConfigStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = ConfigStore::open_with_config_and_artifact_index_path(
        dir.path().join("config.sqlite3"),
        dir.path().join("config.toml"),
        dir.path().join("artifacts/index"),
    )
    .await
    .expect("open store");
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![provider],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    (dir, store)
}

#[cfg(test)]
pub(crate) fn provider_config(base_url: &str, api_key: Option<&str>) -> ProviderConfig {
    ProviderConfig {
        id: "openai".to_string(),
        kind: ProviderKind::Openai,
        name: "OpenAI".to_string(),
        base_url: base_url.to_string(),
        api_key: api_key.map(str::to_string),
        api_key_env: None,
        models: vec![provider_test_model("gpt-5.5")],
        default_model: "gpt-5.5".to_string(),
        enabled: true,
    }
}

#[cfg(test)]
pub(crate) fn provider_test_model(id: &str) -> ModelConfig {
    use mai_protocol::{
        ModelCapabilities, ModelReasoningConfig, ModelReasoningVariant, ModelRequestPolicy,
        ModelWireApi,
    };
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens: 272_000,
        max_context_tokens: None,
        effective_context_window_percent: 95,
        output_tokens: 128_000,
        auto_compact_token_limit: None,
        supports_tools: true,
        wire_api: ModelWireApi::Responses,
        capabilities: ModelCapabilities::default(),
        request_policy: ModelRequestPolicy::default(),
        reasoning: Some(ModelReasoningConfig {
            default_variant: Some("medium".to_string()),
            variants: ["minimal", "medium"]
                .into_iter()
                .map(|variant_id| ModelReasoningVariant {
                    id: variant_id.to_string(),
                    label: None,
                    request: json!({
                        "reasoning": {
                            "effort": variant_id
                        }
                    }),
                })
                .collect(),
        }),
        options: Value::Null,
        headers: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{
        ProviderKind, ProviderTestRequest, ServiceEvent, ServiceEventKind, TokenUsage,
    };
    use serde_json::{Value, json};
    use std::collections::VecDeque;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Mutex as TokioMutex;

    #[tokio::test]
    async fn provider_test_succeeds_against_mock_responses_server() {
        let (base_url, requests) = start_provider_mock(vec![
            json!({
                "id": "resp_test_1",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "ok" }]
                    }
                ],
                "usage": {
                    "input_tokens": 3,
                    "input_tokens_details": { "cached_tokens": 1 },
                    "output_tokens": 2,
                    "output_tokens_details": { "reasoning_tokens": 1 },
                    "total_tokens": 5
                }
            }),
            json!({
                "id": "resp_test_2",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "ok" }]
                    }
                ],
                "usage": {
                    "input_tokens": 4,
                    "input_tokens_details": { "cached_tokens": 3 },
                    "output_tokens": 2,
                    "output_tokens_details": { "reasoning_tokens": 2 },
                    "total_tokens": 6
                }
            }),
        ])
        .await;
        let (_dir, store) = provider_test_store(provider_config(&base_url, Some("secret"))).await;

        let result = run_provider_test(
            &store,
            "openai",
            ProviderTestRequest {
                model: None,
                reasoning_effort: Some("minimal".to_string()),
                deep: true,
            },
        )
        .await;

        assert_eq!(result.status, axum::http::StatusCode::OK);
        let response = result.response;
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.provider_id, "openai");
        assert_eq!(response.provider_name, "OpenAI");
        assert_eq!(response.provider_kind, ProviderKind::Openai);
        assert_eq!(response.model, "gpt-5.5");
        assert_eq!(response.base_url, base_url);
        assert_eq!(response.output_preview, "ok");
        assert_eq!(
            response.usage.expect("usage"),
            TokenUsage {
                input_tokens: 4,
                cached_input_tokens: 3,
                output_tokens: 2,
                reasoning_output_tokens: 2,
                total_tokens: 6,
            }
        );
        assert_eq!(response.error, None);

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["path"], "/responses");
        assert_eq!(requests[0]["authorization"], "Bearer secret");
        assert_eq!(requests[0]["body"]["model"], "gpt-5.5");
        assert_eq!(requests[0]["body"]["store"], true);
        assert_eq!(
            requests[0]["body"].pointer("/reasoning/effort"),
            Some(&json!("minimal"))
        );
        assert_eq!(requests[1]["body"]["previous_response_id"], "resp_test_1");
        assert_eq!(
            requests[1]["body"].pointer("/reasoning/effort"),
            Some(&json!("minimal"))
        );
    }

    #[tokio::test]
    async fn provider_test_deep_mode_covers_continuation_fallback() {
        let (base_url, requests) = start_provider_mock(vec![
            json!({
                "id": "resp_test_1",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "ok" }]
                    }
                ],
                "usage": { "input_tokens": 3, "output_tokens": 2, "total_tokens": 5 }
            }),
            json!({
                "__status": 400,
                "error": {
                    "message": "previous_response_id is only supported on Responses WebSocket v2",
                    "type": "invalid_request_error"
                }
            }),
            json!({
                "id": "resp_test_2",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "ok" }]
                    }
                ],
                "usage": { "input_tokens": 6, "output_tokens": 2, "total_tokens": 8 }
            }),
        ])
        .await;
        let (_dir, store) = provider_test_store(provider_config(&base_url, Some("secret"))).await;

        let result = run_provider_test(&store, "openai", ProviderTestRequest::default()).await;

        assert_eq!(result.status, axum::http::StatusCode::OK);
        let response = result.response;
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.output_preview, "ok");
        assert_eq!(response.usage.expect("usage").total_tokens, 8);

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert!(requests[0]["body"].get("previous_response_id").is_none());
        assert_eq!(requests[1]["body"]["previous_response_id"], "resp_test_1");
        assert!(requests[2]["body"].get("previous_response_id").is_none());
        assert_eq!(requests[2]["body"]["store"], false);
        assert_eq!(
            requests[2]["body"]["input"]
                .as_array()
                .expect("input")
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn provider_test_reports_missing_provider() {
        let (_dir, store) =
            provider_test_store(provider_config("http://127.0.0.1:1", Some("secret"))).await;

        let result = run_provider_test(&store, "missing", ProviderTestRequest::default()).await;

        assert_eq!(result.status, axum::http::StatusCode::BAD_REQUEST);
        let response = result.response;
        assert!(!response.ok);
        assert_eq!(response.provider_id, "missing");
        assert!(
            response
                .error
                .unwrap()
                .contains("provider `missing` not found")
        );
    }

    #[tokio::test]
    async fn provider_test_reports_missing_api_key_with_provider_context() {
        let (_dir, store) = provider_test_store(provider_config("http://127.0.0.1:1", None)).await;

        let result = run_provider_test(&store, "openai", ProviderTestRequest::default()).await;

        assert_eq!(result.status, axum::http::StatusCode::BAD_REQUEST);
        let response = result.response;
        assert!(!response.ok);
        assert_eq!(response.provider_id, "openai");
        assert_eq!(response.provider_name, "OpenAI");
        assert_eq!(response.model, "gpt-5.5");
        assert_eq!(response.base_url, "http://127.0.0.1:1");
        assert!(
            response
                .error
                .unwrap()
                .contains("provider `openai` has no API key")
        );
    }

    #[tokio::test]
    async fn provider_test_reports_unknown_model_with_provider_context() {
        let (_dir, store) =
            provider_test_store(provider_config("http://127.0.0.1:1", Some("secret"))).await;

        let result = run_provider_test(
            &store,
            "openai",
            ProviderTestRequest {
                model: Some("missing-model".to_string()),
                reasoning_effort: None,
                deep: true,
            },
        )
        .await;

        assert_eq!(result.status, axum::http::StatusCode::BAD_REQUEST);
        let response = result.response;
        assert!(!response.ok);
        assert_eq!(response.provider_id, "openai");
        assert_eq!(response.model, "missing-model");
        assert!(
            response
                .error
                .unwrap()
                .contains("model `missing-model` is not configured for provider `openai`")
        );
    }

    #[tokio::test]
    async fn provider_test_reports_upstream_error_without_leaking_key() {
        let (base_url, _requests) = start_provider_mock(vec![json!({
            "__status": 401,
            "error": {
                "message": "bad token secret-token",
                "type": "invalid_request_error"
            }
        })])
        .await;
        let (_dir, store) =
            provider_test_store(provider_config(&base_url, Some("secret-token"))).await;

        let result = run_provider_test(&store, "openai", ProviderTestRequest::default()).await;

        assert_eq!(result.status, axum::http::StatusCode::OK);
        let response = result.response;
        assert!(!response.ok);
        assert_eq!(response.base_url, base_url);
        let error = response.error.expect("error");
        assert!(error.contains("401 Unauthorized"));
        assert!(error.contains("[redacted]"));
        assert!(
            !error.contains("secret-token"),
            "provider test leaked api key: {error}"
        );
    }

    async fn start_provider_mock(responses: Vec<Value>) -> (String, Arc<TokioMutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock addr");
        let responses = Arc::new(TokioMutex::new(VecDeque::from(responses)));
        let requests = Arc::new(TokioMutex::new(Vec::new()));
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
                    let request = read_provider_mock_request(&mut stream).await;
                    requests.lock().await.push(request);
                    let response = responses.lock().await.pop_front().unwrap_or_else(|| {
                        json!({
                            "id": "resp_empty",
                            "output": [],
                            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
                        })
                    });
                    write_provider_mock_response(&mut stream, response).await;
                });
            }
        });
        (format!("http://{addr}"), requests)
    }

    async fn read_provider_mock_request(stream: &mut TcpStream) -> Value {
        let mut buffer = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let n = stream.read(&mut chunk).await.expect("read request");
            if n == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..n]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                let text = String::from_utf8_lossy(&buffer);
                let header_end = text.find("\r\n\r\n").expect("header end");
                let headers = &text[..header_end];
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let body_start = header_end + 4;
                if buffer.len() >= body_start + content_length {
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&buffer);
        let header_end = text.find("\r\n\r\n").expect("header end");
        let headers = &text[..header_end];
        let body = &buffer[header_end + 4..];
        let path = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default();
        let authorization = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .map(|(_, value)| value.trim().to_string())
            })
            .unwrap_or_default();
        json!({
            "path": path,
            "authorization": authorization,
            "body": serde_json::from_slice::<Value>(body).unwrap_or(Value::Null),
        })
    }

    async fn write_provider_mock_response(stream: &mut TcpStream, mut response: Value) {
        let status = response
            .as_object_mut()
            .and_then(|object| object.remove("__status"))
            .and_then(|value| value.as_u64())
            .unwrap_or(200);
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Status",
        };
        let body = if status == 200 {
            provider_mock_sse_body(&response)
        } else {
            serde_json::to_string(&response).expect("response json")
        };
        let content_type = if status == 200 {
            "text/event-stream"
        } else {
            "application/json"
        };
        let raw = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(raw.as_bytes())
            .await
            .expect("write response");
    }

    fn provider_mock_sse_body(response: &Value) -> String {
        let response_id = response
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("resp_mock");
        let mut events = vec![json!({
            "type": "response.created",
            "response": { "id": response_id }
        })];
        for (index, item) in response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let mut item = item.clone();
            if let Some(object) = item.as_object_mut()
                && object.get("type").and_then(Value::as_str) == Some("message")
            {
                object
                    .entry("id".to_string())
                    .or_insert_with(|| json!(format!("msg_{index}")));
                object
                    .entry("role".to_string())
                    .or_insert_with(|| json!("assistant"));
            }
            events.push(json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": item,
            }));
        }
        events.push(json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "usage": response.get("usage").cloned().unwrap_or(Value::Null),
            }
        }));
        events
            .into_iter()
            .map(|event| {
                let kind = event
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message");
                format!("event: {kind}\ndata: {event}\n\n")
            })
            .collect()
    }

    #[tokio::test]
    async fn service_event_replay_returns_events_after_sequence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = mai_store::ConfigStore::open_with_config_path(
            dir.path().join("server.sqlite3"),
            dir.path().join("config.toml"),
        )
        .await
        .expect("open store");
        for sequence in 1..=3 {
            store
                .append_service_event(&ServiceEvent {
                    sequence,
                    timestamp: mai_protocol::now(),
                    kind: ServiceEventKind::Error {
                        agent_id: None,
                        session_id: None,
                        turn_id: None,
                        message: format!("event {sequence}"),
                    },
                })
                .await
                .expect("append event");
        }

        let replay = store.service_events_after(1, 10).await.expect("replay");
        assert_eq!(
            replay
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }
}
