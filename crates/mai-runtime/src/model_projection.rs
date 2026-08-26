use mai_protocol::{ModelOutputItem, ModelResponse, TokenUsage};

pub fn completion_response_usage(usage: &pl_model::TokenUsage) -> TokenUsage {
    let snapshot = pl_core::ModelTokenUsageSnapshot::from(usage);
    model_token_usage(&snapshot)
}

pub fn model_token_usage(snapshot: &pl_core::ModelTokenUsageSnapshot) -> TokenUsage {
    TokenUsage {
        prompt_tokens: snapshot.input_tokens(),
        cached_prompt_tokens: snapshot.cached_input_tokens(),
        cache_write_tokens: snapshot.cache_write_tokens(),
        completion_tokens: snapshot.output_tokens(),
        reasoning_tokens: snapshot.reasoning_output_tokens(),
        total_tokens: snapshot.total_tokens(),
    }
}

pub fn completion_response_to_model_response(
    response: pl_model::CompletionResponse,
) -> ModelResponse {
    let snapshot = pl_core::completion_response_snapshot(&response);
    let output = snapshot
        .output()
        .iter()
        .map(|item| {
            if let Some(content) = item.as_reasoning() {
                return ModelOutputItem::Reasoning {
                    content: content.to_string(),
                };
            }
            if let Some(text) = item.as_message() {
                return ModelOutputItem::Message {
                    text: text.to_string(),
                };
            }
            if let Some(function_call) = item.as_function_call() {
                return ModelOutputItem::FunctionCall {
                    call_id: function_call.call_id().to_string(),
                    name: function_call.name().to_string(),
                    arguments: function_call.arguments().clone(),
                    raw_arguments: function_call.raw_arguments().to_string(),
                };
            }
            unreachable!("pl-core response output snapshot has no visible projection")
        })
        .collect();
    ModelResponse {
        id: snapshot.id().map(ToString::to_string),
        output,
        usage: Some(model_token_usage(snapshot.usage())),
    }
}
