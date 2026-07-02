use mai_protocol::{ModelOutputItem, ModelResponse, TokenUsage};

pub fn completion_response_preview(response: &pl_model::CompletionResponse) -> String {
    let mut parts = Vec::new();
    if let Some(reasoning) = response
        .reasoning_content
        .as_deref()
        .filter(|text| !text.is_empty())
    {
        parts.push(reasoning.to_string());
    }
    if let Some(content) = response.content.as_deref().filter(|text| !text.is_empty()) {
        parts.push(content.to_string());
    }
    parts.extend(response.tool_calls.iter().map(|call| {
        format!(
            "function_call {} {}: {}",
            call.name,
            call.call_id.as_deref().unwrap_or(&call.id),
            call.payload_text()
        )
    }));
    mai_protocol::preview(&parts.join("\n"), 500)
}

pub fn completion_response_usage(usage: &pl_model::TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.prompt_tokens,
        cached_input_tokens: usage.cached_prompt_tokens,
        output_tokens: usage.completion_tokens,
        reasoning_output_tokens: usage.reasoning_tokens,
        total_tokens: usage.total_tokens,
    }
}

pub fn completion_response_to_model_response(
    response: pl_model::CompletionResponse,
) -> ModelResponse {
    let mut output = Vec::new();
    if let Some(reasoning) = response
        .reasoning_content
        .filter(|text| !text.trim().is_empty())
    {
        output.push(ModelOutputItem::Reasoning { content: reasoning });
    }
    if let Some(content) = response.content.filter(|text| !text.trim().is_empty()) {
        output.push(ModelOutputItem::Message { text: content });
    }
    output.extend(response.tool_calls.into_iter().map(|call| {
        let raw_arguments = call.payload_text();
        let arguments = call.arguments_for_tool();
        let call_id = call.call_id.clone().unwrap_or_else(|| call.id.clone());
        ModelOutputItem::FunctionCall {
            call_id,
            name: call.name,
            arguments,
            raw_arguments,
        }
    }));
    ModelResponse {
        id: response.response_id,
        output,
        usage: Some(completion_response_usage(&response.usage)),
    }
}
