use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use mai_protocol::{
    ModelCapabilities as MaiModelCapabilities, ModelConfig, ModelReasoningVariant, ModelWireApi,
    ProviderKind as MaiProviderKind, ProviderSecret, ToolDefinition,
};
use pl_core::CoreSession;
use pl_model::{
    CompletionEventStream, CompletionRequest, CompletionResponse, CompletionStreamAccumulator,
    MaxTokensField, ModelCapabilities, ModelInfo, ModelModality, ModelParameter, ModelProvider,
    ModelRequestProfile, ParameterWire, ProviderInfo, ReasoningConfig, ReasoningSummary,
    SharedModelProvider, ToolCapabilities, ToolSchema, WireAssignment, create_provider_with_models,
    is_continuation_unsupported_error,
};
use pl_protocol::PureError;
use serde_json::{Map, Value};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

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

    pub(crate) fn provider_for_selection(
        selection: &mai_store::ProviderSelection,
    ) -> Result<SharedModelProvider, PureError> {
        let mut info = provider_info(&selection.provider);
        info.default_model = selection.model.id.clone();
        create_provider_with_models(info, vec![model_info(&selection.model)])
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

fn provider_info(provider: &ProviderSecret) -> ProviderInfo {
    let mut info = match provider.kind {
        MaiProviderKind::Openai => ProviderInfo::openai(Some(provider.base_url.clone())),
        MaiProviderKind::Deepseek => ProviderInfo::deepseek(Some(provider.base_url.clone())),
        MaiProviderKind::Zhipu => ProviderInfo::zhipu(Some(provider.base_url.clone())),
        MaiProviderKind::Mimo => ProviderInfo::openai_compatible_chat(
            provider.name.clone(),
            provider.base_url.clone(),
            provider.default_model.clone(),
        ),
    };
    info.name = provider.name.clone();
    info.default_model = provider.default_model.clone();
    info.bearer_token = Some(provider.api_key.clone());
    info
}

fn model_info(model: &ModelConfig) -> ModelInfo {
    let mut info = ModelInfo::fallback(&model.id);
    info.display_name = model.name.clone().unwrap_or_else(|| model.id.clone());
    info.context_window = Some(model.context_tokens);
    info.max_context_window = model.max_context_tokens.or(Some(model.context_tokens));
    info.auto_compact_token_limit = model.auto_compact_token_limit;
    info.max_output_tokens = Some(model.output_tokens);
    info.capabilities = model_capabilities(&model.capabilities, model.supports_tools);
    info.capabilities.reasoning = model.reasoning.is_some();
    info.parameters = reasoning_parameters(model);
    info.request_profile = request_profile(model);
    info
}

fn model_capabilities(
    capabilities: &MaiModelCapabilities,
    supports_tools: bool,
) -> ModelCapabilities {
    ModelCapabilities {
        streaming: true,
        temperature: true,
        reasoning: capabilities.reasoning_replay,
        web_search: false,
        input: vec![ModelModality::Text],
        output: vec![ModelModality::Text],
        tools: ToolCapabilities {
            function_calling: supports_tools && capabilities.tools,
            parallel_tool_calls: capabilities.parallel_tools,
            custom_tools: false,
            freeform_tools: false,
        },
        interleaved: None,
    }
}

fn request_profile(model: &ModelConfig) -> ModelRequestProfile {
    let mut body = Map::new();
    merge_object(&mut body, &model.options);
    merge_object(&mut body, &model.request_policy.extra_body);
    ModelRequestProfile {
        api_model: Some(model.id.clone()),
        headers: model
            .headers
            .iter()
            .chain(model.request_policy.headers.iter())
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<HashMap<_, _>>(),
        body,
        options: Map::new(),
        max_tokens_field: match model.request_policy.max_tokens_field.as_str() {
            "max_completion_tokens" => MaxTokensField::MaxCompletionTokens,
            "max_tokens" => MaxTokensField::MaxTokens,
            _ => MaxTokensField::MaxTokens,
        },
    }
}

fn merge_object(target: &mut Map<String, Value>, value: &Value) {
    if let Some(object) = value.as_object() {
        target.extend(
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
}

fn completion_request(
    model: &ModelConfig,
    reasoning_effort: Option<&str>,
    instructions: String,
    tools: &[ToolDefinition],
    session: &CoreSession,
    use_continuation: bool,
) -> CompletionRequest {
    let messages = if use_continuation {
        session
            .continuation_start_index()
            .and_then(|start| session.messages().get(start..))
            .unwrap_or_else(|| session.messages())
            .to_vec()
    } else {
        session.messages().to_vec()
    };
    let previous_response_id = use_continuation
        .then(|| session.previous_response_id().map(ToString::to_string))
        .flatten();
    CompletionRequest::builder(model.id.clone())
        .instructions(instructions)
        .messages(messages)
        .store(Some(use_continuation))
        .previous_response_id(previous_response_id)
        .prompt_cache_key(session.prompt_cache_key().map(ToString::to_string))
        .tools(tools.iter().map(tool_schema).collect())
        .parallel_tool_calls(model.capabilities.parallel_tools)
        .max_tokens(model.output_tokens)
        .reasoning(reasoning_config(model, reasoning_effort))
        .build()
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

fn reasoning_config(
    model: &ModelConfig,
    reasoning_effort: Option<&str>,
) -> Option<ReasoningConfig> {
    let config = model.reasoning.as_ref()?;
    let effort = reasoning_effort
        .map(ToString::to_string)
        .or_else(|| default_reasoning_effort(&config.variants));
    Some(ReasoningConfig {
        effort,
        summary: Some(ReasoningSummary::Auto),
    })
}

fn default_reasoning_effort(variants: &[ModelReasoningVariant]) -> Option<String> {
    variants.first().map(|variant| variant.id.clone())
}

fn reasoning_parameters(model: &ModelConfig) -> Vec<ModelParameter> {
    let Some(config) = model.reasoning.as_ref() else {
        return Vec::new();
    };
    if config.variants.is_empty() {
        return Vec::new();
    }

    let mut wire = BTreeMap::new();
    for variant in &config.variants {
        wire.insert(
            variant.id.clone(),
            ParameterWire {
                set: wire_assignments(&variant.request),
                remove: Vec::new(),
            },
        );
    }

    vec![ModelParameter {
        name: "effort".to_string(),
        label: Some("Reasoning effort".to_string()),
        candidates: config
            .variants
            .iter()
            .map(|variant| variant.id.clone())
            .collect(),
        wire,
    }]
}

fn wire_assignments(request: &Value) -> Vec<WireAssignment> {
    let mut assignments = Vec::new();
    collect_wire_assignments(None, request, &mut assignments);
    assignments
}

fn collect_wire_assignments(
    prefix: Option<&str>,
    value: &Value,
    assignments: &mut Vec<WireAssignment>,
) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                let path = prefix
                    .map(|prefix| format!("{prefix}.{key}"))
                    .unwrap_or_else(|| key.clone());
                collect_wire_assignments(Some(&path), value, assignments);
            }
        }
        Value::Null => {}
        Value::Array(_) | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            if let Some(path) = prefix {
                assignments.push(WireAssignment {
                    path: path.to_string(),
                    value: value.clone(),
                });
            }
        }
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
