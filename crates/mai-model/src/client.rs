use crate::error::{ModelError, Result};
use crate::provider::{ProviderResolver, ResolvedProvider, response_id_unsupported_for_responses_http};
use crate::types::{ModelEventStream, ModelStreamEvent, ModelTurnState};
use crate::wire::responses::openai_turn_input;
use crate::wire::{WireRequest, parse_sse_frames};
use async_stream::try_stream;
use futures::StreamExt;
use mai_protocol::{ModelInputItem, ToolDefinition};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

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

    pub async fn send_turn(
        &self,
        resolved: &ResolvedProvider,
        instructions: &str,
        input: &[ModelInputItem],
        tools: &[ToolDefinition],
        state: &mut ModelTurnState,
        cancellation_token: &CancellationToken,
    ) -> Result<ModelEventStream> {
        if cancellation_token.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        if self.continuation_is_unsupported(&resolved.cache_key).await {
            state.continuation_disabled = true;
            state.previous_response_id = None;
        }

        let use_continuation = resolved.supports_continuation && !state.continuation_disabled;
        let (request_input, previous_response_id, store) = if use_continuation {
            let (delta_input, previous_response_id) = openai_turn_input(input, state);
            (delta_input, previous_response_id, Some(true))
        } else {
            (input, None, Some(false))
        };

        let wire_req = WireRequest {
            model_id: &resolved.model_id,
            instructions,
            input: request_input,
            tools,
            tool_choice: tool_choice(tools, resolved.supports_tools),
            stream: true,
            store,
            previous_response_id,
            max_output_tokens: resolved.max_output_tokens,
            extra_body: resolved.extra_body.clone(),
            supports_tools: resolved.supports_tools,
        };

        match self
            .open_stream_request(resolved, &wire_req, cancellation_token)
            .await
        {
            Ok(response) => Ok(self.response_stream(
                resolved.clone_for_stream(),
                response,
                state,
                cancellation_token.clone(),
            )),
            Err(err)
                if previous_response_id.is_some()
                    && response_id_unsupported_for_responses_http(&err) =>
            {
                self.mark_continuation_unsupported(&resolved.cache_key).await;
                state.continuation_disabled = true;
                state.previous_response_id = None;
                let retry_req = WireRequest {
                    model_id: &resolved.model_id,
                    instructions,
                    input,
                    tools,
                    tool_choice: tool_choice(tools, resolved.supports_tools),
                    stream: true,
                    store: Some(false),
                    previous_response_id: None,
                    max_output_tokens: resolved.max_output_tokens,
                    extra_body: resolved.extra_body.clone(),
                    supports_tools: resolved.supports_tools,
                };
                let response = self
                    .open_stream_request(resolved, &retry_req, cancellation_token)
                    .await?;
                Ok(self.response_stream(
                    resolved.clone_for_stream(),
                    response,
                    state,
                    cancellation_token.clone(),
                ))
            }
            Err(err) => Err(err),
        }
    }

    async fn open_stream_request(
        &self,
        resolved: &ResolvedProvider,
        wire_req: &WireRequest<'_>,
        cancellation_token: &CancellationToken,
    ) -> Result<reqwest::Response> {
        let body = resolved.wire_protocol.build_body(wire_req)?;
        let max_retries = 5u32;
        for attempt in 0..=max_retries {
            if cancellation_token.is_cancelled() {
                return Err(ModelError::Cancelled);
            }
            let send = self
                .http
                .post(&resolved.endpoint)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .bearer_auth(&resolved.api_key)
                .headers(resolved.headers.clone())
                .body(body.clone())
                .send();
            let response = tokio::select! {
                response = send => response.map_err(|source| ModelError::Request {
                    endpoint: resolved.endpoint.clone(),
                    source,
                })?,
                _ = cancellation_token.cancelled() => return Err(ModelError::Cancelled),
            };
            let status = response.status();
            if status.is_server_error() && attempt < max_retries {
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
                continue;
            }
            if !status.is_success() {
                let text = response.text().await.map_err(|source| ModelError::Request {
                    endpoint: resolved.endpoint.clone(),
                    source,
                })?;
                return Err(ModelError::Api {
                    endpoint: resolved.endpoint.clone(),
                    status,
                    body: text,
                });
            }
            return Ok(response);
        }
        unreachable!()
    }

    fn response_stream(
        &self,
        resolved: StreamResolvedProvider,
        response: reqwest::Response,
        _state: &mut ModelTurnState,
        cancellation_token: CancellationToken,
    ) -> ModelEventStream {
        let mut body = response.bytes_stream();
        let wire_protocol = resolved.wire_protocol;
        Box::pin(try_stream! {
            let mut buffer = Vec::new();
            let mut seen_completed = false;
            loop {
                if cancellation_token.is_cancelled() {
                    Err(ModelError::Cancelled)?;
                }
                let next_chunk: Result<Option<_>> = tokio::select! {
                    chunk = timeout(STREAM_IDLE_TIMEOUT, body.next()) => {
                        chunk.map_err(|_| ModelError::Stream("idle timeout waiting for SSE".to_string()))
                    }
                    _ = cancellation_token.cancelled() => Err(ModelError::Cancelled),
                };
                let next_chunk = next_chunk?;
                let Some(chunk) = next_chunk else {
                    break;
                };
                let chunk = chunk.map_err(|source| ModelError::Request {
                    endpoint: resolved.endpoint.clone(),
                    source,
                })?;
                for frame in parse_sse_frames(&mut buffer, &chunk)? {
                    if frame.data.trim() == "[DONE]" {
                        let events = wire_protocol.parse_stream_done()?;
                        for event in events {
                            if matches!(event, ModelStreamEvent::Completed { .. }) {
                                seen_completed = true;
                            }
                            yield event;
                            if seen_completed {
                                break;
                            }
                        }
                        if seen_completed {
                            break;
                        }
                        continue;
                    }
                    let events = wire_protocol.parse_stream_event(&frame)?;
                    for event in events {
                        if matches!(event, ModelStreamEvent::Completed { .. }) {
                            seen_completed = true;
                        }
                        yield event;
                        if seen_completed {
                            break;
                        }
                    }
                    if seen_completed {
                        break;
                    }
                }
                if seen_completed {
                    break;
                }
            }
            if !buffer.iter().all(u8::is_ascii_whitespace) {
                Err(ModelError::Stream("stream closed with incomplete SSE frame".to_string()))?;
            }
            if !seen_completed {
                Err(ModelError::Stream("stream closed before response.completed".to_string()))?;
            }
        })
    }

    pub fn apply_completed_state(&self, state: &mut ModelTurnState, response_id: Option<&str>) {
        if let Some(id) = response_id {
            if state.continuation_disabled {
                state.previous_response_id = None;
            } else {
                state.previous_response_id = Some(id.to_string());
            }
        }
    }

    async fn continuation_is_unsupported(&self, cache_key: &str) -> bool {
        self.continuation_cache.lock().await.contains(cache_key)
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

#[derive(Clone)]
struct StreamResolvedProvider {
    endpoint: String,
    wire_protocol: Arc<dyn crate::wire::WireProtocol>,
}

trait CloneForStream {
    fn clone_for_stream(&self) -> StreamResolvedProvider;
}

impl CloneForStream for ResolvedProvider {
    fn clone_for_stream(&self) -> StreamResolvedProvider {
        StreamResolvedProvider {
            endpoint: self.endpoint.clone(),
            wire_protocol: Arc::clone(&self.wire_protocol),
        }
    }
}
