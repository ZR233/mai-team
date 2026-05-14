use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use mai_model::{
    ModelClient, ModelError, ModelStreamAccumulator, ModelTurnState, ResolvedProvider,
};
use mai_protocol::*;
use mai_store::ConfigStore;
use tokio_util::sync::CancellationToken;

use super::helpers::{elapsed_millis, model_output_preview, sanitize_provider_test_error};
use super::state::{ApiError, AppState};

pub(crate) async fn get_providers(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<ProvidersResponse>, ApiError> {
    Ok(Json(state.store.providers_response().await?))
}

pub(crate) async fn save_providers(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ProvidersConfigRequest>,
) -> std::result::Result<Json<ProvidersResponse>, ApiError> {
    state.store.save_providers(request).await?;
    Ok(Json(state.store.providers_response().await?))
}

pub(crate) async fn get_mcp_servers(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<McpServersConfigRequest>, ApiError> {
    Ok(Json(McpServersConfigRequest {
        servers: state.store.list_mcp_servers().await?,
    }))
}

pub(crate) async fn save_mcp_servers(
    State(state): State<Arc<AppState>>,
    Json(request): Json<McpServersConfigRequest>,
) -> std::result::Result<Json<McpServersConfigRequest>, ApiError> {
    state.store.save_mcp_servers(&request.servers).await?;
    Ok(Json(McpServersConfigRequest {
        servers: state.store.list_mcp_servers().await?,
    }))
}

pub(crate) async fn test_provider(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<ProviderTestRequest>,
) -> std::result::Result<Response, ApiError> {
    let result = run_provider_test(&state.store, &id, request).await;
    Ok((result.status, Json(result.response)).into_response())
}

pub(crate) struct ProviderTestResult {
    pub(crate) status: StatusCode,
    pub(crate) response: ProviderTestResponse,
}

pub(crate) async fn run_provider_test(
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

    let provider = selection.provider;
    let model = selection.model;
    let base_url = provider.base_url.clone();
    let reasoning_effort = request.reasoning_effort;
    let client = ModelClient::new();
    let resolved = client.resolve(&provider, &model, reasoning_effort.as_deref());
    let response = if request.deep && resolved.supports_continuation {
        run_provider_deep_model_test(&client, &resolved, reasoning_effort).await
    } else {
        let input = [ModelInputItem::user_text("ping")];
        let mut state = ModelTurnState::default();
        let cancellation_token = CancellationToken::new();
        consume_model_stream_to_response(
            &client,
            &resolved,
            "You are a provider connectivity test. Reply with exactly: ok",
            &input,
            &[],
            &mut state,
            &cancellation_token,
        )
        .await
    };
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
                output_preview: model_output_preview(&response),
                usage: response.usage,
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

pub(crate) async fn run_provider_deep_model_test(
    client: &ModelClient,
    resolved: &ResolvedProvider,
    _reasoning_effort: Option<String>,
) -> std::result::Result<ModelResponse, ModelError> {
    let cancellation_token = CancellationToken::new();
    let mut state = ModelTurnState::default();
    let first_input = vec![ModelInputItem::user_text(
        "Provider deep connectivity test, step 1. Reply exactly: ok",
    )];
    let instructions = "You are a provider connectivity test. Reply with exactly: ok";
    let first = consume_model_stream_to_response(
        client,
        resolved,
        instructions,
        &first_input,
        &[],
        &mut state,
        &cancellation_token,
    )
    .await?;
    let mut second_input = first_input;
    second_input.push(ModelInputItem::assistant_text(model_output_preview(&first)));
    second_input.push(ModelInputItem::user_text(
        "Provider deep connectivity test, step 2. Reply exactly: ok",
    ));
    state.acknowledge_history_len(2);
    consume_model_stream_to_response(
        client,
        resolved,
        instructions,
        &second_input,
        &[],
        &mut state,
        &cancellation_token,
    )
    .await
}

pub(crate) async fn consume_model_stream_to_response(
    client: &ModelClient,
    resolved: &ResolvedProvider,
    instructions: &str,
    input: &[ModelInputItem],
    tools: &[mai_protocol::ToolDefinition],
    state: &mut ModelTurnState,
    cancellation_token: &CancellationToken,
) -> std::result::Result<ModelResponse, ModelError> {
    let mut stream = client
        .send_turn(
            resolved,
            instructions,
            input,
            tools,
            state,
            cancellation_token,
        )
        .await?;
    let mut accumulator = ModelStreamAccumulator::default();
    while let Some(event) = stream.next().await {
        if cancellation_token.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        let event = event?;
        accumulator.push(&event);
    }
    let response = accumulator.finish()?;
    client.apply_completed_state(state, response.id.as_deref());
    Ok(response)
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
    use mai_protocol::{ModelCapabilities, ModelReasoningConfig, ModelReasoningVariant, ModelRequestPolicy, ModelWireApi};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens: 400_000,
        output_tokens: 128_000,
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
