use std::sync::Arc;

use mai_protocol::AgentId;
use pl_core::{
    AgentExecutionPolicy, AgentId as FrameworkAgentId, AgentKernel, AgentRuntimeHandle,
    CoreAgentProfile, TurnEngineBuilder,
};
use pl_model::ToolSchema;

use crate::state::AgentRecord;
use crate::{AgentRuntime, Result};

pub(crate) struct MaiFrameworkKernelBuildContext {
    pub(crate) runtime: Arc<AgentRuntime>,
    pub(crate) agent: Arc<AgentRecord>,
    pub(crate) agent_id: AgentId,
    pub(crate) framework_agent_id: FrameworkAgentId,
    pub(crate) framework_runtime: AgentRuntimeHandle,
    pub(crate) policy: AgentExecutionPolicy,
    pub(crate) product_tool_schemas: Vec<ToolSchema>,
    pub(crate) mcp_lease: Option<pl_core::McpTurnLease>,
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

/// 为 PL Agent Runtime 构造 mai kernel；协作工具直接持有 runtime handle。
pub(crate) async fn build_mai_framework_kernel(
    builder: TurnEngineBuilder,
    runtime_profile: CoreAgentProfile,
    ctx: MaiFrameworkKernelBuildContext,
) -> Result<AgentKernel> {
    let product_tool_registry = super::product_tools::MaiProductToolRegistry::new(
        ctx.runtime.clone(),
        ctx.agent.clone(),
        ctx.agent_id,
        ctx.product_tool_schemas,
    );
    let product_tools = product_tool_registry.registered_tools()?;
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
    let mcp_backend = Arc::new(super::mcp_resources::MaiMcpResourceBackend::new(
        ctx.runtime.clone(),
        ctx.agent.clone(),
        ctx.mcp_lease.clone(),
    ));
    let tool_set = pl_core::ToolSetBuilder::host_provided(capabilities)
        .with_allowed_tools(ctx.policy.visible_tools.iter().cloned())
        .with_command_backend(command_backend)
        .with_workspace_file_backend(workspace_file_backend)
        .with_mcp_resource_tools(mcp_backend);
    let collaboration_tools = pl_core::AgentCollaborationTools::new(
        ctx.framework_runtime,
        ctx.framework_agent_id,
        ctx.policy.collaboration,
    )
    .tools();
    let kernel_builder = AgentKernel::builder(builder)
        .with_profile(runtime_profile)
        .with_tools(collaboration_tools)
        .with_registered_tools(product_tools);
    let mut kernel = if let Some(git_runtime) = git_runtime {
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
    if let Some(lease) = ctx.mcp_lease {
        lease.install_tools(kernel.core_mut())?;
    }
    Ok(kernel)
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
