use mai_protocol::{ModelOutputItem, ModelResponse, TokenUsageSnapshot};

pub fn completion_response_usage(usage: &pl_protocol::UsageReport) -> TokenUsageSnapshot {
    usage.totals().public_snapshot()
}

pub fn model_token_usage(accounting: &pl_protocol::InferenceAccounting) -> TokenUsageSnapshot {
    completion_response_usage(&accounting.usage)
}

pub fn completion_response_to_model_response(
    response: pl_model::completion::CompletionResponse,
) -> ModelResponse {
    let snapshot = pl_model::completion::completion_response_snapshot(&response);
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
            unreachable!("pl-model response output snapshot has no visible projection")
        })
        .collect();
    ModelResponse {
        id: snapshot.id().map(ToString::to_string),
        output,
        usage: Some(model_token_usage(snapshot.accounting())),
    }
}
