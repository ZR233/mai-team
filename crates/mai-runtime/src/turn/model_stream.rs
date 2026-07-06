use mai_protocol::{ModelResponse, ToolDefinition};
use mai_store::ProviderSelection;
use pl_core::CoreSession;
use pl_protocol::{Message, PureError};
use tokio_util::sync::CancellationToken;

use crate::{ModelClient, completion_response_to_model_response};

#[derive(Debug)]
pub(crate) struct TurnModelContext {
    pub(crate) provider_id: String,
    pub(crate) model_name: String,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) provider_selection: ProviderSelection,
    pub(crate) tools: Vec<ToolDefinition>,
    pub(crate) instructions: String,
}

pub(crate) async fn consume_model_stream_to_response(
    model: &ModelClient,
    provider_selection: &ProviderSelection,
    reasoning_effort: Option<&str>,
    instructions: &str,
    input: &[Message],
    tools: &[ToolDefinition],
    cancellation_token: &CancellationToken,
) -> std::result::Result<ModelResponse, PureError> {
    let mut session = CoreSession::from_messages(input.to_vec());
    let response = model
        .stream_session_completion_response(
            provider_selection,
            reasoning_effort,
            instructions,
            tools,
            &mut session,
            cancellation_token,
        )
        .await?;
    Ok(completion_response_to_model_response(response))
}
