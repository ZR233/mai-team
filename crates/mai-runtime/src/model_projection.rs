use mai_protocol::{ModelOutputItem, ModelResponse, TokenUsage};

pub fn completion_response_usage(usage: &pl_model::TokenUsage) -> TokenUsage {
    let snapshot = pl_core::ModelTokenUsageSnapshot::from_model_usage(usage);
    token_usage_from_snapshot(&snapshot)
}

fn token_usage_from_snapshot(snapshot: &pl_core::ModelTokenUsageSnapshot) -> TokenUsage {
    TokenUsage {
        input_tokens: snapshot.input_tokens(),
        cached_input_tokens: snapshot.cached_input_tokens(),
        output_tokens: snapshot.output_tokens(),
        reasoning_output_tokens: snapshot.reasoning_output_tokens(),
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
        usage: Some(token_usage_from_snapshot(snapshot.usage())),
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

    #[test]
    fn completion_usage_projection_uses_pl_core_snapshot_accessors() {
        let source = include_str!("model_projection.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");

        for accessor in [
            "snapshot.input_tokens()",
            "snapshot.cached_input_tokens()",
            "snapshot.output_tokens()",
            "snapshot.reasoning_output_tokens()",
            "snapshot.total_tokens()",
        ] {
            assert!(
                production.contains(accessor),
                "模型 usage 投影应通过 pl-core accessor `{accessor}`"
            );
        }
        for forbidden in [
            "snapshot.input_tokens,",
            "snapshot.cached_input_tokens,",
            "snapshot.output_tokens,",
            "snapshot.reasoning_output_tokens,",
            "snapshot.total_tokens,",
        ] {
            assert!(
                !production.contains(forbidden),
                "mai-runtime 不应直接读取 pl-core usage snapshot 字段 `{forbidden}`"
            );
        }
    }

    #[test]
    fn completion_response_projection_reuses_pl_core_snapshot() {
        let source = include_str!("model_projection.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");

        assert!(
            production.contains("pl_core::completion_response_snapshot"),
            "CompletionResponse output 语义应由 pl-core completion_response_snapshot 统一提供"
        );
        for forbidden in [
            "response.reasoning_content",
            "response.content",
            "response.tool_calls",
            "call.payload_text",
            "call.arguments_for_tool",
            "call.call_id.clone",
            "call.id.clone",
            "call.name.clone",
        ] {
            assert!(
                !production.contains(forbidden),
                "mai-runtime 不应直接解释 pl_model::CompletionResponse 字段或 tool call `{forbidden}`"
            );
        }
    }

    #[test]
    fn completion_response_projection_uses_pl_core_snapshot_accessors() {
        let source = include_str!("model_projection.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");

        for accessor in [
            "snapshot.id()",
            ".output()",
            "snapshot.usage()",
            ".as_reasoning()",
            ".as_message()",
            ".as_function_call()",
        ] {
            assert!(
                production.contains(accessor),
                "模型响应投影应通过 pl-core accessor `{accessor}`"
            );
        }
        for forbidden in [
            "snapshot.output.into_iter()",
            "snapshot.id,",
            "snapshot.usage)",
            "CompletionResponseOutputSnapshot::Reasoning",
            "CompletionResponseOutputSnapshot::Message",
            "CompletionResponseOutputSnapshot::FunctionCall",
        ] {
            assert!(
                !production.contains(forbidden),
                "mai-runtime 不应依赖 pl-core response snapshot 内部结构 `{forbidden}`"
            );
        }
    }
}
