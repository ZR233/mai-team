use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use mai_protocol::{ModelConfig, ModelWireApi, ProviderKind as MaiProviderKind, ToolDefinition};
use pl_core::CoreSession;
use pl_model::{
    CompletionEventStream, CompletionRequest, CompletionResponse, CompletionStreamAccumulator,
    ModelContinuationState, ModelProvider, SharedModelProvider, ToolSchema,
    create_provider_with_models, is_continuation_unsupported_error,
};
use pl_protocol::PureError;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::model_profile::{model_info, provider_info, reasoning_config};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct ModelClient {
    continuation_cache: Arc<Mutex<HashSet<String>>>,
    #[allow(dead_code)]
    config: ModelClientConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelClientConfig {
    pub connect_timeout: Duration,
    pub response_timeout: Duration,
    pub stream_idle_timeout: Duration,
}

impl Default for ModelClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            response_timeout: DEFAULT_RESPONSE_TIMEOUT,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
        }
    }
}

impl ModelClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(config: ModelClientConfig) -> Self {
        Self {
            continuation_cache: Arc::new(Mutex::new(HashSet::new())),
            config,
        }
    }

    pub async fn prepare_turn(
        &self,
        selection: &mai_store::ProviderSelection,
        reasoning_effort: Option<&str>,
        instructions: String,
        tools: &[ToolDefinition],
        session: &mut CoreSession,
    ) -> Result<PreparedModelTurn, PureError> {
        if self
            .continuation_is_unsupported(&continuation_cache_key(selection))
            .await
        {
            session.mark_continuation_unsupported();
        }

        let provider = create_provider_with_models(
            provider_info(&selection.provider),
            vec![model_info(&selection.model)],
        )?;
        let use_continuation =
            Self::supports_continuation(selection) && !session.continuation_disabled();
        let request = completion_request(
            &selection.model,
            reasoning_effort,
            instructions,
            tools,
            session,
            use_continuation,
        );
        Ok(PreparedModelTurn { provider, request })
    }

    pub fn apply_completed_state(&self, session: &mut CoreSession, response_id: Option<&str>) {
        session.acknowledge_model_response(session.len(), response_id.map(ToString::to_string));
    }

    pub async fn stream_session_completion_response(
        &self,
        selection: &mai_store::ProviderSelection,
        reasoning_effort: Option<&str>,
        instructions: &str,
        tools: &[ToolDefinition],
        session: &mut CoreSession,
        cancellation_token: &CancellationToken,
    ) -> Result<CompletionResponse, PureError> {
        let mut stream = self
            .open_completion_event_stream(selection, reasoning_effort, instructions, tools, session)
            .await?;
        let (event_tx, _event_rx) = tokio::sync::broadcast::channel(8);
        let mut accumulator = CompletionStreamAccumulator::new(None);
        while let Some(event) = stream.next().await {
            if cancellation_token.is_cancelled() {
                return Err(PureError::LlmError("request cancelled".to_string()));
            }
            let event = event?;
            accumulator.apply(event, &event_tx)?;
        }
        let response = accumulator.finish(&event_tx)?;
        self.apply_completed_state(session, response.response_id.as_deref());
        Ok(response)
    }

    pub async fn open_completion_event_stream(
        &self,
        selection: &mai_store::ProviderSelection,
        reasoning_effort: Option<&str>,
        instructions: &str,
        tools: &[ToolDefinition],
        session: &mut CoreSession,
    ) -> Result<CompletionEventStream, PureError> {
        let mut prepared = self
            .prepare_turn(
                selection,
                reasoning_effort,
                instructions.to_string(),
                tools,
                session,
            )
            .await?;
        let had_continuation = prepared.request.previous_response_id.is_some();
        match prepared.provider.stream_events(prepared.request).await {
            Ok(stream) => Ok(stream),
            Err(err) if had_continuation && is_continuation_unsupported_error(&err) => {
                self.mark_continuation_unsupported(selection, session).await;
                prepared = self
                    .prepare_turn(
                        selection,
                        reasoning_effort,
                        instructions.to_string(),
                        tools,
                        session,
                    )
                    .await?;
                prepared.provider.stream_events(prepared.request).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn mark_continuation_unsupported(
        &self,
        selection: &mai_store::ProviderSelection,
        session: &mut CoreSession,
    ) {
        self.continuation_cache
            .lock()
            .await
            .insert(continuation_cache_key(selection));
        session.mark_continuation_unsupported();
    }

    pub fn supports_continuation(selection: &mai_store::ProviderSelection) -> bool {
        selection.provider.kind == MaiProviderKind::Openai
            && selection.model.wire_api == ModelWireApi::Responses
            && selection.model.capabilities.continuation
    }

    async fn continuation_is_unsupported(&self, key: &str) -> bool {
        self.continuation_cache.lock().await.contains(key)
    }
}

impl Default for ModelClient {
    fn default() -> Self {
        Self::with_config(ModelClientConfig::default())
    }
}

pub struct PreparedModelTurn {
    pub provider: SharedModelProvider,
    pub request: CompletionRequest,
}

fn completion_request(
    model: &ModelConfig,
    reasoning_effort: Option<&str>,
    instructions: String,
    tools: &[ToolDefinition],
    session: &CoreSession,
    use_continuation: bool,
) -> CompletionRequest {
    CompletionRequest::builder(model.id.clone())
        .instructions(instructions)
        .messages_from_continuation(
            session.messages(),
            &model_continuation_state(session),
            use_continuation,
        )
        .tools(tools.iter().map(tool_schema).collect())
        .parallel_tool_calls(model.capabilities.parallel_tools)
        .max_tokens(model.output_tokens)
        .reasoning(reasoning_config(model, reasoning_effort))
        .build()
}

fn model_continuation_state(session: &CoreSession) -> ModelContinuationState {
    let mut continuation = ModelContinuationState::default();
    if let Some(prompt_cache_key) = session.prompt_cache_key() {
        continuation.set_prompt_cache_key(prompt_cache_key.to_string());
    }
    if session.continuation_disabled() {
        continuation.mark_unsupported();
        return continuation;
    }
    continuation.acknowledge_response(
        session.acknowledged_message_count(),
        session.previous_response_id().map(ToString::to_string),
        session.len(),
    );
    continuation
}

fn tool_schema(tool: &ToolDefinition) -> ToolSchema {
    match tool.kind.as_str() {
        "function" => ToolSchema::function(
            tool.name.clone(),
            tool.description.clone(),
            tool.parameters.clone(),
        ),
        _ => ToolSchema::function(
            tool.name.clone(),
            tool.description.clone(),
            tool.parameters.clone(),
        ),
    }
}

fn continuation_cache_key(selection: &mai_store::ProviderSelection) -> String {
    format!(
        "{:?}|{}|{}",
        selection.provider.kind,
        selection.provider.base_url.trim_end_matches('/'),
        selection.model.id
    )
}
