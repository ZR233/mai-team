use std::sync::Arc;
use std::time::Instant;

use axum::http::StatusCode;
use mai_protocol::*;
use mai_runtime::model_token_usage;
#[cfg(test)]
use mai_store::MaiStore;
use pl_core::{
    AgentSession, CompletionResponseSnapshot, ModelTurnClient, ModelTurnOptions, ModelTurnRequest,
    ResolvedModelRoute, user_text_message,
};
use pl_model::ModelTransportProfile;
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
    runtime: Arc<mai_runtime::AgentRuntime>,
}

impl ProviderService {
    pub(crate) fn new(runtime: Arc<mai_runtime::AgentRuntime>) -> Self {
        Self { runtime }
    }

    pub(crate) async fn providers_response(
        &self,
    ) -> Result<ProvidersResponse, mai_runtime::RuntimeError> {
        self.runtime.providers_response().await
    }

    pub(crate) async fn save_providers(
        &self,
        request: ProvidersConfigRequest,
    ) -> Result<ProvidersResponse, mai_runtime::RuntimeError> {
        self.runtime.update_providers(request).await
    }

    pub(crate) async fn web_search(
        &self,
    ) -> Result<WebSearchSettingsResponse, mai_runtime::RuntimeError> {
        self.runtime.web_search_settings().await
    }

    pub(crate) async fn save_web_search(
        &self,
        request: WebSearchSettings,
    ) -> Result<WebSearchSettingsResponse, mai_runtime::RuntimeError> {
        self.runtime.update_web_search_settings(request).await
    }

    pub(crate) async fn mcp_servers(
        &self,
    ) -> Result<McpServersResponse, mai_runtime::RuntimeError> {
        self.runtime.mcp_servers_response().await
    }

    pub(crate) async fn save_mcp_servers(
        &self,
        request: McpServersConfigRequest,
    ) -> Result<McpServersResponse, mai_runtime::RuntimeError> {
        self.runtime.update_mcp_servers(request).await
    }

    pub(crate) async fn save_builtin_mcp_servers(
        &self,
        request: BuiltinMcpServersRequest,
    ) -> Result<McpServersResponse, mai_runtime::RuntimeError> {
        self.runtime.update_builtin_mcp_servers(request).await
    }

    pub(crate) async fn recheck_mcp_servers(
        &self,
    ) -> Result<McpServersResponse, mai_runtime::RuntimeError> {
        self.runtime.recheck_mcp_servers().await
    }

    pub(crate) async fn test_provider(
        &self,
        provider_id: &str,
        request: ProviderTestRequest,
    ) -> ProviderTestResult {
        run_provider_test(&self.runtime, provider_id, request).await
    }
}

pub(crate) struct ProviderTestResult {
    pub(crate) status: StatusCode,
    pub(crate) response: ProviderTestResponse,
}

async fn run_provider_test(
    runtime: &mai_runtime::AgentRuntime,
    provider_id: &str,
    request: ProviderTestRequest,
) -> ProviderTestResult {
    let started = Instant::now();
    let selection = match runtime
        .resolve_provider_selection(
            Some(provider_id),
            request.model.as_deref(),
            request.reasoning_effort.as_deref(),
        )
        .await
    {
        Ok(selection) => selection,
        Err(err) => {
            let provider = runtime
                .providers_response()
                .await
                .ok()
                .and_then(|response| {
                    response
                        .providers
                        .into_iter()
                        .find(|provider| provider.id == provider_id)
                });
            let model = request.model.clone().or_else(|| {
                provider
                    .as_ref()
                    .and_then(|provider| provider.models.first())
                    .map(|model| model.slug.clone())
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
                        .map(|provider| provider.config.name.clone())
                        .unwrap_or_default(),
                    transport: provider
                        .as_ref()
                        .and_then(|provider| provider.models.first())
                        .map(|model| model.transport.clone())
                        .unwrap_or_else(ModelTransportProfile::default),
                    model: model.unwrap_or_default(),
                    base_url: provider
                        .as_ref()
                        .map(|provider| provider.config.base_url.clone())
                        .unwrap_or_default(),
                    latency_ms: elapsed_millis(started),
                    output_preview: String::new(),
                    usage: None,
                    error: Some(err.to_string()),
                },
            };
        }
    };

    let provider_id = selection.provider_id.to_string();
    let provider_name = selection.endpoint.name.clone();
    let base_url = selection.endpoint.base_url.clone();
    let api_key = selection.endpoint.bearer_token.clone().unwrap_or_default();
    let transport = selection.model.transport.clone();
    let model = selection.model.slug.clone();
    let tester = ProviderTester::new();
    let response = tester.run_test(&selection, request.deep).await;
    let latency_ms = elapsed_millis(started);
    match response {
        Ok(response) => ProviderTestResult {
            status: StatusCode::OK,
            response: ProviderTestResponse {
                ok: true,
                provider_id,
                provider_name,
                transport,
                model,
                base_url,
                latency_ms,
                output_preview: completion_snapshot_preview(&response),
                usage: Some(model_token_usage(response.usage())),
                error: None,
            },
        },
        Err(err) => ProviderTestResult {
            status: StatusCode::OK,
            response: ProviderTestResponse {
                ok: false,
                provider_id,
                provider_name,
                transport,
                model,
                base_url,
                latency_ms,
                output_preview: String::new(),
                usage: None,
                error: Some(sanitize_provider_test_error(&err, &api_key)),
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
        selection: &ResolvedModelRoute,
        deep: bool,
    ) -> std::result::Result<CompletionResponseSnapshot, PureError> {
        if deep {
            self.run_deep_test(selection).await
        } else {
            self.run_single_test(selection).await
        }
    }

    async fn run_single_test(
        &self,
        selection: &ResolvedModelRoute,
    ) -> std::result::Result<CompletionResponseSnapshot, PureError> {
        let session = AgentSession::from_messages(vec![user_text_message("ping")]);
        let client = ModelTurnClient::from_route(selection)?;
        client
            .complete(
                &session,
                provider_test_request(
                    selection,
                    "You are a provider connectivity test. Reply with exactly: ok",
                ),
                ModelTurnOptions::default().with_cancellation(CancellationToken::new()),
            )
            .await
    }

    async fn run_deep_test(
        &self,
        selection: &ResolvedModelRoute,
    ) -> std::result::Result<CompletionResponseSnapshot, PureError> {
        let client = ModelTurnClient::from_route(selection)?;
        let mut session = AgentSession::from_messages(vec![user_text_message(
            "Provider deep connectivity test, step 1. Reply exactly: ok",
        )]);
        let instructions = "You are a provider connectivity test. Reply with exactly: ok";
        let first = client
            .complete(
                &session,
                provider_test_request(selection, instructions),
                ModelTurnOptions::default().with_cancellation(CancellationToken::new()),
            )
            .await?;
        session.push_assistant_response(completion_snapshot_text(&first), None);
        session.push_user_prompt(
            "Provider deep connectivity test, step 2. Reply exactly: ok".to_string(),
        );
        client
            .complete(
                &session,
                provider_test_request(selection, instructions),
                ModelTurnOptions::default().with_cancellation(CancellationToken::new()),
            )
            .await
    }
}

fn provider_test_request(selection: &ResolvedModelRoute, instructions: &str) -> ModelTurnRequest {
    ModelTurnRequest::from_route(selection).with_instructions(instructions)
}

fn completion_snapshot_text(response: &CompletionResponseSnapshot) -> String {
    response
        .output()
        .iter()
        .filter_map(pl_core::CompletionResponseOutputSnapshot::as_message)
        .collect::<Vec<_>>()
        .join("")
}

fn completion_snapshot_preview(response: &CompletionResponseSnapshot) -> String {
    mai_protocol::preview(&completion_snapshot_text(response), 1_500)
}

#[cfg(test)]
pub(crate) async fn provider_test_store(
    provider: ProviderConfig,
) -> (tempfile::TempDir, Arc<mai_runtime::AgentRuntime>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        MaiStore::open_with_config_and_artifact_index_path(
            dir.path().join("config.sqlite3"),
            dir.path().join("config.toml"),
            dir.path().join("artifacts/index"),
        )
        .await
        .expect("open store"),
    );
    let provider_id = provider.id.clone();
    let selected_model = match &provider.source {
        ProviderConfigSource::Preset { preset_id, .. } => {
            let preset = pl_core::builtin_provider_catalog()
                .presets
                .into_iter()
                .find(|preset| preset.id.as_str() == preset_id)
                .expect("provider preset");
            preset
                .provider
                .effective_models()
                .expect("provider models")
                .into_iter()
                .find(|model| model.slug == preset.suggested_model)
                .expect("suggested model")
        }
        ProviderConfigSource::Custom { config } => config
            .effective_models()
            .expect("provider models")
            .into_iter()
            .next()
            .expect("provider model"),
    };
    let effort = selected_model
        .default_effort()
        .map(pl_core::ReasoningEffort::new);
    let route = pl_core::ModelRouteConfig {
        provider: pl_core::ProviderId::new(&provider_id).expect("provider id"),
        model: selected_model.slug,
        effort,
    };
    let agents = AgentConfigRequest {
        planner: Some(route.clone()),
        explorer: Some(route.clone()),
        executor: Some(route.clone()),
        reviewer: Some(route),
    };
    let models = mai_runtime::model_config_from_api(
        &ProvidersConfigRequest {
            providers: vec![provider],
        },
        &agents,
    )
    .expect("model config");
    let config = mai_runtime::MaiConfig {
        models,
        ..mai_runtime::MaiConfig::default()
    };
    store
        .config_documents()
        .save(&config)
        .await
        .expect("save config");
    let runtime = mai_runtime::AgentRuntime::new(
        mai_docker::DockerClient::new("unused-image"),
        store,
        mai_runtime::RuntimeConfig {
            repo_root: dir.path().to_path_buf(),
            projects_root: dir.path().join("projects"),
            cache_root: dir.path().join("cache"),
            artifact_files_root: dir.path().join("artifacts/files"),
            sidecar_image: "unused-sidecar".to_string(),
            github_api_base_url: None,
            git_binary: None,
            system_skills_root: None,
            system_agents_root: None,
        },
    )
    .await
    .expect("runtime");
    (dir, runtime)
}

#[cfg(test)]
pub(crate) fn provider_config(base_url: &str, api_key: Option<&str>) -> ProviderConfig {
    let mut model = pl_core::builtin_provider_catalog()
        .presets
        .into_iter()
        .find(|preset| preset.id.as_str() == "openai")
        .expect("OpenAI preset")
        .provider
        .effective_models()
        .expect("OpenAI models")
        .into_iter()
        .find(|model| model.slug == "gpt-5.5")
        .expect("gpt-5.5 model");
    model.transport = pl_model::ModelTransportProfile::responses_http();
    let mut config = pl_core::ProviderConfig::from_explicit_models(
        pl_model::ProviderEndpoint::openai(Some(base_url.to_string())),
        vec![model],
    );
    config.bearer_token = api_key.map(str::to_string);
    ProviderConfig {
        id: "openai".to_string(),
        source: ProviderConfigSource::Custom { config },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{
        MaiProductEventEnvelope, MaiProductEventKind, ProviderTestRequest, TokenUsage,
    };
    use pl_model::ModelTransportProfile;
    use serde_json::{Value, json};
    use std::collections::{HashMap, VecDeque};
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
                reasoning_effort: Some("low".to_string()),
                deep: true,
            },
        )
        .await;

        assert_eq!(result.status, axum::http::StatusCode::OK);
        let response = result.response;
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.provider_id, "openai");
        assert_eq!(response.provider_name, "OpenAI");
        assert_eq!(response.transport, ModelTransportProfile::responses_http());
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
        assert_eq!(requests[0]["body"]["store"], false);
        assert_eq!(
            requests[0]["body"].pointer("/reasoning/effort"),
            Some(&json!("low"))
        );
        assert!(requests[1]["body"].get("previous_response_id").is_none());
        assert_eq!(
            requests[1]["body"]["input"]
                .as_array()
                .expect("input")
                .len(),
            3
        );
        assert_eq!(
            requests[1]["body"].pointer("/reasoning/effort"),
            Some(&json!("low"))
        );
    }

    #[tokio::test]
    async fn provider_test_deep_mode_uses_full_history_over_responses_http() {
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
        assert_eq!(requests.len(), 2);
        assert!(requests[0]["body"].get("previous_response_id").is_none());
        assert!(requests[1]["body"].get("previous_response_id").is_none());
        assert_eq!(requests[1]["body"]["store"], false);
        for field in ["max_tokens", "max_output_tokens", "max_completion_tokens"] {
            assert!(requests[0]["body"].get(field).is_none());
            assert!(requests[1]["body"].get(field).is_none());
        }
        assert_eq!(
            requests[1]["body"]["input"]
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

    #[tokio::test]
    async fn provider_update_preserves_secret_headers_and_full_mai_config_document() {
        let mut original = provider_config("https://old.example/v1", Some("secret"));
        let ProviderConfigSource::Custom { config } = &mut original.source else {
            panic!("custom provider")
        };
        config.http_headers = Some(HashMap::from([(
            "x-openai-actor-authorization".to_string(),
            "local-image-extension".to_string(),
        )]));
        let (directory, runtime) = provider_test_store(original).await;
        let mut provider = provider_config("https://old.example/v1", None);
        let ProviderConfigSource::Custom { config } = &mut provider.source else {
            panic!("custom provider")
        };
        config.name = "Renamed OpenAI".to_string();
        config.http_headers = None;

        let response = runtime
            .update_providers(ProvidersConfigRequest {
                providers: vec![provider],
            })
            .await
            .expect("update providers");

        assert!(response.providers[0].has_api_key);
        assert_eq!(response.providers[0].config.name, "Renamed OpenAI");
        let document = mai_store::ConfigDocumentStore::new(directory.path().join("config.toml"));
        let config = document
            .load::<mai_runtime::MaiConfig>()
            .await
            .expect("load config document")
            .expect("config document");
        config.validate().expect("valid full mai config");
        assert_eq!(config.containers.turn_cancel_grace_ms, 500);
        let stored_provider = config
            .models
            .providers
            .get(&pl_core::ProviderId::new("openai").expect("provider id"))
            .expect("stored provider");
        assert_eq!(stored_provider.bearer_token.as_deref(), Some("secret"));
        assert_eq!(
            stored_provider.http_headers,
            Some(std::collections::HashMap::from([(
                "x-openai-actor-authorization".to_string(),
                "local-image-extension".to_string(),
            )]))
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
    async fn product_event_replay_returns_events_after_sequence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = mai_store::MaiStore::open_with_config_path(
            dir.path().join("server.sqlite3"),
            dir.path().join("config.toml"),
        )
        .await
        .expect("open store");
        for sequence in 1..=3 {
            store
                .append_product_event(&MaiProductEventEnvelope {
                    sequence,
                    timestamp: mai_protocol::now(),
                    kind: MaiProductEventKind::OperationFailed {
                        scope: "test".to_string(),
                        agent_id: None,
                        message: format!("event {sequence}"),
                    },
                })
                .await
                .expect("append event");
        }

        let replay = store.product_events_after(1, 10).await.expect("replay");
        assert_eq!(
            replay
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }
}
