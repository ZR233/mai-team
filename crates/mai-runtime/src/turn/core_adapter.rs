use std::sync::Arc;

use mai_protocol::AgentId;
use pl_core::{
    AgentExecutionPolicy, AgentRuntimeHandle, AgentToolSet, CoreRuntimeProfile,
    ThreadId as FrameworkThreadId, ToolGroupId, TurnEngine, TurnEngineBuilder,
};

use crate::state::AgentRecord;
use crate::{AgentRuntime, Result};

/// mai-team 产品动作工具的原生工具组标识。
pub(crate) const PRODUCT_TOOL_GROUP: &str = "mai-product";

pub(crate) struct MaiFrameworkKernelBuildContext {
    pub(crate) runtime: Arc<AgentRuntime>,
    pub(crate) agent: Arc<AgentRecord>,
    pub(crate) agent_id: AgentId,
    pub(crate) framework_agent_id: FrameworkThreadId,
    pub(crate) framework_runtime: AgentRuntimeHandle,
    pub(crate) policy: AgentExecutionPolicy,
    pub(crate) agent_tools: AgentToolSet,
    pub(crate) skill_catalog: Option<Arc<pl_core::skill::FrozenSkillCatalog>>,
    pub(crate) exclusive_web_search: bool,
}

/// 为 PL Agent Runtime 构造 mai turn engine。
///
/// 每个产品 Agent 只绑定一个持久 `AgentToolSet`。产品、技能与协作能力都以原生工具组
/// 安装；exclusive Web Search 直接卸载其余组，不保留名称过滤或双路径注册。
pub(crate) async fn build_mai_turn_engine(
    builder: TurnEngineBuilder,
    runtime_profile: CoreRuntimeProfile,
    ctx: MaiFrameworkKernelBuildContext,
) -> Result<TurnEngine> {
    let mut engine = builder
        .with_agent_tool_set(ctx.agent_tools.clone())
        .with_runtime_profile(runtime_profile)
        .build();
    if ctx.exclusive_web_search {
        for group in [
            "builtin",
            PRODUCT_TOOL_GROUP,
            "skills",
            "collaboration",
            "mcp",
        ] {
            engine.agent_tools().uninstall(&ToolGroupId::new(group));
        }
        return Ok(engine);
    }

    let summary = ctx.agent.summary.read().await.clone();
    let workspace_root = if summary.project_id.is_some() {
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
        crate::tools::git::native_git_tool_runtime(ctx.runtime.clone(), &ctx.agent).await?;
    let capabilities =
        pl_core::ToolCapabilityConfig::hosted_workspace().with_git(git_runtime.is_some());
    let installer = pl_core::BuiltinToolInstaller::host_provided(capabilities)
        .with_command_backend(command_backend)
        .with_workspace_file_backend(workspace_file_backend);
    if let Some(git_runtime) = git_runtime {
        installer
            .with_git_tools(
                git_runtime.config,
                git_runtime.backend,
                git_runtime.credential_provider,
            )
            .install(&mut engine, workspace_root, None)
            .await?;
    } else {
        installer.install(&mut engine, workspace_root, None).await?;
    }

    let product_tools = super::product_tools::MaiProductTools::new(
        ctx.runtime.clone(),
        ctx.agent.clone(),
        ctx.agent_id,
    )
    .tools(&summary)?;
    engine
        .agent_tools()
        .install(ToolGroupId::new(PRODUCT_TOOL_GROUP), product_tools)?;
    if let Some(catalog) = ctx.skill_catalog {
        engine.install_skill_tools_from_catalog(catalog, pl_core::SkillToolMode::ReadOnly)?;
    } else {
        engine.agent_tools().uninstall(&ToolGroupId::new("skills"));
    }

    let collaboration = pl_core::AgentCollaborationTools::new(
        ctx.framework_runtime,
        ctx.framework_agent_id,
        pl_core::AgentCollaborationToolConfig {
            policy: ctx.policy.collaboration,
            session_runtime: engine.tool_session_runtime(),
            workspace_root: workspace_root.into(),
        },
    );
    engine
        .agent_tools()
        .install(ToolGroupId::new("collaboration"), collaboration.tools())?;
    Ok(engine)
}
