use mai_protocol::{ModelOutputItem, ModelResponse, TokenUsage};

pub fn completion_response_usage(usage: &pl_model::TokenUsage) -> TokenUsage {
    let snapshot = pl_core::ModelTokenUsageSnapshot::from_model_usage(usage);
    TokenUsage {
        input_tokens: snapshot.input_tokens,
        cached_input_tokens: snapshot.cached_input_tokens,
        output_tokens: snapshot.output_tokens,
        reasoning_output_tokens: snapshot.reasoning_output_tokens,
        total_tokens: snapshot.total_tokens,
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

#[cfg(test)]
mod tests {
    #[test]
    fn completion_preview_is_owned_by_pl_core() {
        let source = include_str!("model_projection.rs");

        assert!(
            !source.contains(&format!("{}{}", "pub fn completion", "_response_preview")),
            "completion response preview 应由 pl-core 统一实现"
        );
    }

    #[test]
    fn completion_usage_projection_reuses_pl_core_snapshot() {
        let source = include_str!("model_projection.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");

        assert!(
            production.contains("pl_core::ModelTokenUsageSnapshot::from_model_usage"),
            "模型 usage 字段解释应由 pl-core ModelTokenUsageSnapshot 统一提供"
        );
        for forbidden in [
            "usage.prompt_tokens",
            "usage.cached_prompt_tokens",
            "usage.completion_tokens",
            "usage.reasoning_tokens",
            "usage.total_tokens",
        ] {
            assert!(
                !production.contains(forbidden),
                "mai-runtime 不应直接解释 pl_model::TokenUsage 字段 `{forbidden}`"
            );
        }
    }
}
