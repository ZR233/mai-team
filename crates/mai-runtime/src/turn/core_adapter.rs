use std::sync::Arc;

use mai_protocol::AgentId;
use pl_core::{
    AgentExecutionPolicy, AgentId as FrameworkAgentId, AgentRuntimeHandle, CoreRuntimeProfile,
    NamespaceDescriptor, ToolEntry, ToolRegistry, ToolSourceId, ToolSourceMetadata, TurnEngine,
    TurnEngineBuilder,
};
use crate::state::AgentRecord;
use crate::{AgentRuntime, Result};

/// mai-team 产品动作工具的工具来源标识。
pub(crate) const PRODUCT_TOOL_SOURCE: &str = "mai-product";

pub(crate) struct MaiFrameworkKernelBuildContext {
    pub(crate) runtime: Arc<AgentRuntime>,
    pub(crate) agent: Arc<AgentRecord>,
    pub(crate) agent_id: AgentId,
    pub(crate) framework_agent_id: FrameworkAgentId,
    pub(crate) framework_runtime: AgentRuntimeHandle,
    pub(crate) policy: AgentExecutionPolicy,
    pub(crate) mcp_shared_tools: Option<Arc<ToolRegistry>>,
}

pub(crate) fn mai_user_input_interaction_callback() -> pl_core::InteractionCallback {
    Arc::new(move |interaction| {
        Box::pin(async move {
            match interaction.payload {
                pl_protocol::InteractionPayload::UserInput { .. } => {
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

/// 为 PL Agent Runtime 构造 mai turn engine；协作工具直接持有 runtime handle。
///
/// MCP 工具不在这里装配：它们由 MCP runtime 按 generation 发布到共享
/// `ToolRegistry`，engine 通过 `with_shared_tool_registry` 消费。
pub(crate) async fn build_mai_turn_engine(
    builder: TurnEngineBuilder,
    runtime_profile: CoreRuntimeProfile,
    ctx: MaiFrameworkKernelBuildContext,
) -> Result<TurnEngine> {
    let product_tool_registry = super::product_tools::MaiProductToolRegistry::new(
        ctx.runtime.clone(),
        ctx.agent.clone(),
        ctx.agent_id,
        ctx.policy.visible_tools.clone(),
    );
    let workspace_root = if ctx.agent.summary.read().await.project_id.is_some() {
        crate::projects::workspace::AGENT_WORKSPACE_REPO_PATH
    } else {
        "/workspace"
    };
    let workspace_backend = Arc::new(super::container::MaiContainerBackend::new(
        ctx.runtime.clone(),
        ctx.agent_id,
    ));
    let workspace_file_backend = Arc::new(pl_core::ContainerWorkspaceFileBackend::new(
        workspace_backend,
    ));
    let command_backend = Arc::new(super::command::MaiCommandBackend::new(
        ctx.runtime.clone(),
        ctx.agent_id,
        workspace_root,
    ));
    let git_runtime =
        crate::tools::git::native_git_tool_runtime(ctx.runtime.clone(), &ctx.agent, |name| {
            ctx.policy.visible_tools.contains(name)
        })
        .await?;
    let capabilities =
        pl_core::ToolCapabilityConfig::hosted_workspace().with_git(git_runtime.is_some());
    let tool_set = pl_core::ToolSetBuilder::host_provided(capabilities)
        .with_allowed_tools(ctx.policy.visible_tools.iter().cloned())
        .with_command_backend(command_backend)
        .with_workspace_file_backend(workspace_file_backend);
    let mut builder = builder.with_runtime_profile(runtime_profile);
    if let Some(shared_tools) = ctx.mcp_shared_tools {
        builder = builder.with_shared_tool_registry(shared_tools);
    }
    let mut engine = builder.build();
    if let Some(git_runtime) = git_runtime {
        tool_set
            .with_git_tools(
                git_runtime.config,
                git_runtime.backend,
                git_runtime.credential_provider,
            )
            .register(&mut engine, workspace_root, None)
            .await;
    } else {
        tool_set.register(&mut engine, workspace_root, None).await;
    }
    let collaboration_source = ToolSourceId::collaboration();
    let collaboration = pl_core::AgentCollaborationTools::new(
        ctx.framework_runtime,
        ctx.framework_agent_id,
        ctx.policy.collaboration.clone(),
    );
    let collaboration_entries = collaboration
        .tools()
        .into_iter()
        .map(|tool| {
            ToolEntry::from_arc(
                tool,
                ToolSourceMetadata::new(collaboration_source.clone()).with_namespace(
                    NamespaceDescriptor::new(
                        "agents",
                        "Subagent discovery, messaging, waiting, and lifecycle tools.",
                    ),
                ),
            )
        })
        .collect::<Vec<_>>();
    engine.register_source_tools(collaboration_source, collaboration_entries)?;
    let product_source = ToolSourceId::new(PRODUCT_TOOL_SOURCE);
    let product_entries = product_tool_registry
        .registered_tools()?
        .into_iter()
        .map(|tool| ToolEntry::new(tool, ToolSourceMetadata::new(product_source.clone())))
        .collect::<Vec<_>>();
    engine.register_source_tools(product_source, product_entries)?;
    let skill_source = ToolSourceId::new(super::skill_resources::SKILL_TOOL_SOURCE);
    let skill_entries = super::skill_resources::skill_resource_entries(
        ctx.runtime.clone(),
        ctx.agent.clone(),
    );
    engine.register_source_tools(skill_source, skill_entries)?;
    Ok(engine)
}

#[cfg(test)]
mod tests {
    #[test]
    fn collaboration_tools_only_receive_non_generic_runtime_handle() {
        let source = include_str!("core_adapter.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();

        assert!(production.contains("AgentCollaborationTools::new"));
        assert!(!production.contains("with_agent_control_tools"));
        assert!(!production.contains("MaiAgentControlBackend"));
    }
}
