use std::time::Duration;

use mai_protocol::{ModelWireApi, ProviderKind as MaiProviderKind, ToolDefinition};
use pl_core::{CoreModelTurnClient, CoreModelTurnOptions, CoreModelTurnRequest, CoreSession};
use pl_model::{CompletionResponse, SharedModelProvider, ToolSchema, create_provider_with_models};
use pl_protocol::PureError;
use tokio_util::sync::CancellationToken;

use crate::model_profile;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct ModelClient {
    core_model: CoreModelTurnClient,
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
            core_model: CoreModelTurnClient::new(),
            config,
        }
    }

    pub(crate) fn provider_for_selection(
        selection: &mai_store::ProviderSelection,
    ) -> Result<SharedModelProvider, PureError> {
        let mut info = model_profile::provider_info(&selection.provider);
        info.default_model = selection.model.id.clone();
        create_provider_with_models(info, vec![model_profile::model_info(&selection.model)])
    }

    pub fn prepare_turn(
        &self,
        selection: &mai_store::ProviderSelection,
        reasoning_effort: Option<&str>,
        instructions: String,
        tools: &[ToolDefinition],
        session: &mut CoreSession,
    ) -> Result<CoreModelTurnRequest, PureError> {
        let use_continuation =
            Self::supports_continuation(selection) && !session.continuation_disabled();
        let mut request = CoreModelTurnRequest::new(selection.model.id.clone())
            .with_instructions(instructions)
            .with_tools(tools.iter().map(tool_schema).collect())
            .with_parallel_tool_calls(selection.model.capabilities.parallel_tools)
            .with_max_tokens(Some(selection.model.output_tokens))
            .with_reasoning(model_profile::reasoning_config(
                &selection.model,
                reasoning_effort,
            ))
            .with_continuation(use_continuation);
        if Self::supports_continuation(selection) {
            request = request.with_continuation_cache_key(continuation_cache_key(selection));
        }
        Ok(request)
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
        let request = self.prepare_turn(
            selection,
            reasoning_effort,
            instructions.to_string(),
            tools,
            session,
        )?;
        let provider = Self::provider_for_selection(selection)?;
        self.core_model
            .stream_session_completion_response(
                provider,
                session,
                request,
                CoreModelTurnOptions::default().with_cancellation(cancellation_token.clone()),
            )
            .await
    }

    pub fn supports_continuation(selection: &mai_store::ProviderSelection) -> bool {
        selection.provider.kind == MaiProviderKind::Openai
            && selection.model.wire_api == ModelWireApi::Responses
            && selection.model.capabilities.continuation
    }
}

impl Default for ModelClient {
    fn default() -> Self {
        Self::with_config(ModelClientConfig::default())
    }
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
