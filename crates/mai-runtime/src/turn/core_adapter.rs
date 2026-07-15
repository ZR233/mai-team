use std::sync::Arc;

use mai_protocol::{AgentId, ServiceEventKind, SessionId, TurnId};
use pl_core::{AgentKernel, CoreAgentProfile, PureCoreBuilder, ToolVisibilitySet};
use pl_model::ToolSchema;
use tokio_util::sync::CancellationToken;

use crate::state::AgentRecord;
use crate::{AgentRuntime, Result};

pub(crate) struct PureCoreTurnContext {
    pub(crate) runtime: Arc<AgentRuntime>,
    pub(crate) agent: Arc<AgentRecord>,
    pub(crate) agent_id: AgentId,
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) message: String,
    pub(crate) provider_selection: mai_store::ProviderSelection,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) instructions: String,
    pub(crate) workspace_instructions: Option<String>,
    pub(crate) visible_tool_names: ToolVisibilitySet,
    pub(crate) product_tools: Vec<ToolSchema>,
    pub(crate) mcp_tool_schemas: Vec<ToolSchema>,
    pub(crate) history: Vec<pl_protocol::Message>,
    pub(crate) cancellation_token: CancellationToken,
}

pub(crate) struct SharedToolKernelBuildContext {
    pub(crate) runtime: Arc<AgentRuntime>,
    pub(crate) agent: Arc<AgentRecord>,
    pub(crate) agent_id: AgentId,
    pub(crate) visible_tool_names: ToolVisibilitySet,
    pub(crate) mcp_tool_schemas: Vec<ToolSchema>,
    pub(crate) cancellation_token: CancellationToken,
}

pub(crate) struct MaiAgentKernelBuildContext {
    pub(crate) runtime: Arc<AgentRuntime>,
    pub(crate) agent: Arc<AgentRecord>,
    pub(crate) agent_id: AgentId,
    pub(crate) visible_tool_names: ToolVisibilitySet,
    pub(crate) product_tool_schemas: Vec<ToolSchema>,
    pub(crate) mcp_tool_schemas: Vec<ToolSchema>,
    pub(crate) cancellation_token: CancellationToken,
}

pub(crate) async fn run_pure_core_turn(ctx: PureCoreTurnContext) -> Result<()> {
    super::hosted_runtime::run_hosted_agent_turn(ctx).await
}

pub(crate) fn mai_user_input_interaction_callback(
    runtime: Arc<AgentRuntime>,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
) -> pl_core::InteractionCallback {
    Arc::new(move |interaction| {
        let runtime = runtime.clone();
        Box::pin(async move {
            match interaction.payload {
                pl_protocol::InteractionPayload::UserInput { questions } => {
                    let (header, questions) = user_input_questions_from_pl(questions);
                    runtime
                        .events
                        .publish(ServiceEventKind::UserInputRequested {
                            agent_id,
                            session_id: Some(session_id),
                            turn_id,
                            header,
                            questions,
                        })
                        .await;
                    pl_protocol::InteractionResolution::UserInput {
                        answers: Default::default(),
                    }
                }
                pl_protocol::InteractionPayload::ToolApproval { .. } => {
                    pl_protocol::InteractionResolution::ToolApproval {
                        decision: pl_protocol::ToolApprovalResolution::Denied,
                        reason: Some(
                            "mai-team user input callback does not approve tools".to_string(),
                        ),
                    }
                }
                pl_protocol::InteractionPayload::PlanConfirmation { .. } => {
                    pl_protocol::InteractionResolution::PlanConfirmation {
                        decision: pl_protocol::PlanConfirmationResolution::Dismiss,
                        content: None,
                        reason: Some(
                            "mai-team user input callback does not confirm plans".to_string(),
                        ),
                    }
                }
            }
        })
    })
}

fn user_input_questions_from_pl(
    questions: Vec<pl_protocol::UserQuestion>,
) -> (String, Vec<mai_protocol::UserInputQuestion>) {
    let projection = pl_core::project_user_input_questions(questions);
    let questions = projection
        .questions()
        .iter()
        .map(|question| mai_protocol::UserInputQuestion {
            id: question.id().to_string(),
            question: question.question().to_string(),
            options: question
                .options()
                .iter()
                .map(|option| mai_protocol::UserInputOption {
                    label: option.label().to_string(),
                    description: Some(option.description().to_string()),
                })
                .collect(),
        })
        .collect();
    (projection.header().to_string(), questions)
}

pub(crate) async fn build_kernel_with_native_shared_tools(
    builder: PureCoreBuilder,
    runtime_profile: CoreAgentProfile,
    registered_tools: Vec<pl_core::RegisteredTool>,
    ctx: SharedToolKernelBuildContext,
) -> Result<AgentKernel> {
    let backend = Arc::new(super::container::MaiContainerBackend::new(
        ctx.runtime.clone(),
        ctx.agent_id,
    ));
    let git_runtime =
        crate::tools::git::native_git_tool_runtime(ctx.runtime.clone(), &ctx.agent, |name| {
            ctx.visible_tool_names.contains(name)
        })
        .await?;
    let capabilities =
        pl_core::ToolCapabilityConfig::hosted_container_workspace().with_git(git_runtime.is_some());
    let mcp_backend = Arc::new(super::mcp_resources::MaiMcpResourceBackend::new(
        ctx.runtime.clone(),
        ctx.agent.clone(),
        ctx.agent_id,
        ctx.cancellation_token.clone(),
    ));
    let mcp_tool_backend = Arc::new(super::mcp_tools::MaiMcpToolBackend::new(
        ctx.runtime.clone(),
        ctx.agent.clone(),
        ctx.agent_id,
        ctx.cancellation_token.clone(),
    ));
    let agent_control_backend = Arc::new(super::agent_control::MaiAgentControlBackend::new(
        ctx.runtime.clone(),
        ctx.agent.clone(),
        ctx.agent_id,
        ctx.cancellation_token.clone(),
    ));
    let agent_control_policy = Arc::new(super::agent_control::MaiAgentControlPolicy::new(
        ctx.runtime.clone(),
        ctx.agent.clone(),
        ctx.agent_id,
    ));
    let tool_set = pl_core::ToolSetBuilder::from_capabilities(capabilities)
        .with_allowed_tools(ctx.visible_tool_names.iter().cloned())
        .with_container_tools(backend)
        .with_mcp_resource_tools(mcp_backend)
        .with_mcp_tools(ctx.mcp_tool_schemas, mcp_tool_backend)
        .with_agent_control_tools(agent_control_backend)
        .with_agent_control_policy(agent_control_policy);
    let kernel_builder = AgentKernel::builder(builder)
        .with_profile(runtime_profile)
        .with_registered_tools(registered_tools);
    let kernel = if let Some(git_runtime) = git_runtime {
        kernel_builder
            .with_tool_set(tool_set.with_git_tools(
                git_runtime.config,
                git_runtime.backend,
                git_runtime.credential_provider,
            ))
            .build()
            .await
    } else {
        kernel_builder.with_tool_set(tool_set).build().await
    };
    Ok(kernel)
}

pub(crate) async fn build_mai_agent_kernel(
    builder: PureCoreBuilder,
    runtime_profile: CoreAgentProfile,
    ctx: MaiAgentKernelBuildContext,
) -> Result<AgentKernel> {
    let product_tool_registry = super::product_tools::MaiProductToolRegistry::new(
        ctx.runtime.clone(),
        ctx.agent.clone(),
        ctx.agent_id,
        ctx.product_tool_schemas,
    );
    let product_tools = product_tool_registry.registered_tools()?;
    build_kernel_with_native_shared_tools(
        builder,
        runtime_profile,
        product_tools,
        SharedToolKernelBuildContext {
            runtime: ctx.runtime,
            agent: ctx.agent,
            agent_id: ctx.agent_id,
            visible_tool_names: ctx.visible_tool_names,
            mcp_tool_schemas: ctx.mcp_tool_schemas,
            cancellation_token: ctx.cancellation_token,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn context_summary_preview_delegates_to_pl_core() {
        let source = include_str!("hosted_runtime.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production section");

        assert!(production.contains("pl_core::text_preview_chars"));
        assert!(
            !production.contains("fn preview("),
            "context compaction summary preview 不应在 mai-runtime 复制文本截断 helper"
        );
    }

    #[test]
    fn turn_runtime_statistics_use_latest_pl_core_result_shape() {
        let source = include_str!("hosted_runtime.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production section");

        assert!(
            production.contains("result.context_compactions.last()")
                && production.contains("result.last_context_tokens")
                && production.contains("result.usage.total_tokens > 0"),
            "usage/context/compaction 统计应适配最新 pl-core TurnResult 公开字段"
        );
        assert!(!production.contains("result.runtime_snapshot()"));
    }

    #[test]
    fn user_input_projection_uses_pl_core_projection() {
        let source = include_str!("core_adapter.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production section");

        assert!(
            production.contains("pl_core::project_user_input_questions"),
            "request_user_input 的 header/options 归一化应由 pl-core projection 提供"
        );
        for forbidden in [
            "question.header.trim()",
            ".options.unwrap_or_default()",
            "unwrap_or_else(|| \"Input\".to_string())",
        ] {
            assert!(
                !production.contains(forbidden),
                "mai-runtime 不应手写 request_user_input 投影 `{forbidden}`"
            );
        }
    }

    #[test]
    fn raw_instruction_snapshot_uses_pl_core_constructor() {
        let source = include_str!("hosted_runtime.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production section");

        assert!(
            production.contains("InstructionSnapshot::profile_base_override"),
            "宿主 base prompt 快照应由 pl-core 构造，mai-runtime 不应直接拼 InstructionSnapshot 结构"
        );
        for forbidden in [
            "InstructionBlock",
            "InstructionSource",
            "InstructionSourceKind::ProfileBaseOverride",
        ] {
            assert!(
                !production.contains(forbidden),
                "mai-runtime 不应直接依赖 pl-core instruction 内部结构 `{forbidden}`"
            );
        }
    }
}
