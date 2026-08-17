use pl_core::{CoreModelTurnRequest, ResolvedModelRoute};
use pl_model::{
    MissingCandidatePolicy, ModelInfo, ModelParameterCandidateRequest, ReasoningConfig,
    ReasoningSummary, SharedModelProvider, ToolSchema, create_provider_with_catalog,
};
use pl_protocol::PureError;

/// 使用 PL 已解析路由创建可执行 provider，不再重建 provider 或模型元数据。
pub fn core_provider_for_selection(
    selection: &ResolvedModelRoute,
) -> Result<SharedModelProvider, PureError> {
    create_provider_with_catalog(selection.provider_info.clone(), selection.models.clone())
}

/// 使用 PL 模型值对象构造一次轻量模型请求。
pub fn core_model_turn_request(
    selection: &ResolvedModelRoute,
    reasoning_effort: Option<&str>,
    instructions: impl Into<String>,
    tools: Vec<ToolSchema>,
) -> CoreModelTurnRequest {
    CoreModelTurnRequest::new(selection.model.slug.clone())
        .with_instructions(instructions)
        .with_tools(tools)
        .with_parallel_tool_calls(selection.model.capabilities.tools.parallel_tool_calls)
        .with_max_tokens(selection.model.max_output_tokens)
        .with_reasoning(reasoning_config(
            &selection.model,
            reasoning_effort,
            selection.effort.as_ref().map(|effort| effort.as_str()),
        ))
}

pub(crate) fn reasoning_config(
    model: &ModelInfo,
    requested_effort: Option<&str>,
    configured_effort: Option<&str>,
) -> Option<ReasoningConfig> {
    let parameter = model.effort_parameter()?;
    let effort = parameter
        .resolve_candidate(ModelParameterCandidateRequest {
            requested: requested_effort,
            default_candidate: configured_effort,
            missing: MissingCandidatePolicy::UseDefault,
            disabled_values: &["none"],
        })
        .ok()
        .flatten();
    Some(ReasoningConfig {
        effort,
        summary: Some(ReasoningSummary::Auto),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn mai_does_not_rebuild_pl_provider_or_model_semantics() {
        let source = include_str!("model_profile.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        for forbidden in [
            "ModelInfo::fallback",
            "ProviderInfo::openai",
            "responses_websocket",
            "chat_completions_http",
            "wire_assignments_from_value",
            "ProviderConnectionMode",
            "ProviderWireProtocol",
        ] {
            assert!(
                !production.contains(forbidden),
                "mai-runtime 不应重建 PL 模型语义 `{forbidden}`"
            );
        }
    }
}
