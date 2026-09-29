//! 产品 Thread 的工具装配入口。
//!
//! PL core 要求每个 Thread 在装配时一次性转移自己独占的工具实例和 hosted tool 声明。本模块是
//! 产品事实与 [`crate::turn::core_adapter::MaiThreadToolContext`] 之间唯一的装配点：它从 Agent、
//! 容器、MCP、Skill 与产品配置读出真实后端，再交给 `turn::core_adapter` 生成完整工具目录。工具
//! 实现全部来自 pl-tool 与 `turn` 中的产品绑定，本模块不实现任何工具语义。
//!
//! 契约：
//! - 输入只有产品已经解析完成的事实（运行时、Agent 记录、Thread id）；容器、MCP、Skill 与项目
//!   状态在本模块内读取，配置与会话 SQLite 都不交给 PL core；
//! - `registrations` 是一次性转移所有权，每个调用都返回全新的 executor，不能复制或跨 Thread 复用；
//! - `hosted_tools` 只包含 provider 侧执行的声明（如 web search），不混入本地工具；
//! - 装配失败返回产品错误，由装配入口在发布 owner 前关闭未转移的资源，不留下半成品 Thread；
//! - 能力不由配置或依赖决定时不允许静默缺失：缺少物理后端或搜索规划都返回显式错误；协作工具组
//!   只有 Review Thread 这一种明确的例外，它按产品语义不挂载父子协作。

use std::sync::Arc;

use pl_core::tool::opaque::Registration;
use pl_model::runtime::HostedTool;
use tokio_util::sync::CancellationToken;

use crate::config::MaiConfig;
use crate::state::AgentRecord;
use crate::tools::git::native_git_tool_runtime;
use crate::turn::core_adapter::{
    CollaborationAvailability, MaiThreadToolContext, assemble_thread_tools as assemble_tool_catalog,
};
use crate::turn::tool_sets::{MaiSearchBinding, SearchVisibility};
use crate::{AgentRuntime, Result, RuntimeError};

mod tool_mcp;
mod tool_workspace;

use tool_workspace::MaiThreadWorkspaceTools;

pub(crate) use tool_workspace::child_workspace_assignment;

/// 一次 Thread 工具装配的输入；只含产品已经解析完成的事实。
pub(crate) struct ThreadToolRequest {
    /// 产品运行时所有者；装配时用于读取容器、MCP、Skill 与产品配置。
    pub(crate) runtime: Arc<AgentRuntime>,
    pub(crate) agent: Arc<AgentRecord>,
    pub(crate) thread_id: String,
}

/// 一个 Thread 独占的完整工具集。
#[derive(Debug, Default)]
pub(crate) struct ThreadToolAssembly {
    pub(crate) hosted_tools: Vec<HostedTool>,
    pub(crate) registrations: Vec<Registration>,
}

/// 请求装配一个产品 Thread 的工具。
///
/// # Errors
/// 容器、MCP、Skill、Git 或搜索绑定不可用，或任一注册项构造失败时返回产品错误；调用方必须把失败
/// 当作装配失败处理，不能发布一个缺少声明工具的 Thread。
pub(crate) async fn assemble_thread_tools(
    request: ThreadToolRequest,
) -> Result<ThreadToolAssembly> {
    let ThreadToolRequest {
        runtime,
        agent,
        thread_id,
    } = request;
    let summary = agent.summary.read().await.clone();
    let agent_id = summary.id;

    // 命令、文件与 Git 都在 Agent 容器内执行，MCP runtime 也随容器启动；先确保容器驻留，缺少物理
    // 后端时在这里显式失败，而不是装配一个之后必然报错的工具目录。
    runtime.container_id(agent_id).await?;

    let config = runtime.mai_config.read().await.clone();
    let workspace = Arc::new(MaiThreadWorkspaceTools::new(
        Arc::clone(&runtime),
        agent_id,
        &summary,
    ));
    let git = native_git_tool_runtime(Arc::clone(&runtime), &agent).await?;
    let skill_catalog = skill_catalog(&runtime, &agent, &config).await?;
    *agent.skill_catalog.write().await = skill_catalog.clone();
    let mcp = tool_mcp::registrations(&runtime, &agent).await?;
    // Review 身份来自创建时写入的产品事实；Review Context 在 Thread 装配之后才附加，
    // 不能用它判定是否是 Review Thread，否则装配会把 Review Thread 误判成普通 Thread。
    let collaboration = collaboration_availability(&runtime, summary.review_run_id.is_some());
    let search = search_binding(&config, &summary)?;

    let catalog = assemble_tool_catalog(MaiThreadToolContext {
        runtime,
        agent,
        agent_id,
        workspace,
        git,
        skill_catalog,
        mcp,
        collaboration,
        search,
    })
    .await?;
    let (hosted_tools, registrations) = catalog.into_parts();
    tracing::debug!(
        agent_id = %agent_id,
        thread_id = %thread_id,
        hosted_tools = hosted_tools.len(),
        registrations = registrations.len(),
        "assembled product thread tools"
    );
    Ok(ThreadToolAssembly {
        hosted_tools,
        registrations,
    })
}

/// 冻结本 Thread 唯一的 Skill 目录。
///
/// Skill 目录按 Agent 的 project/review 来源选择，并在项目 skill 读锁内一次性冻结；
/// `config.skills.enabled` 为 false 时不挂载 Skill 工具（这是配置决定，不是缺失能力）。
async fn skill_catalog(
    runtime: &Arc<AgentRuntime>,
    agent: &Arc<AgentRecord>,
    config: &MaiConfig,
) -> Result<Option<Arc<pl_tool::skill::FrozenSkillCatalog>>> {
    if !config.skills.enabled {
        return Ok(None);
    }
    if let Err(error) = runtime.refresh_project_skills_for_agent(agent).await {
        tracing::warn!(error = %error, "failed to refresh project skills before Thread assembly");
    }
    let request = runtime.deps.store.load_skills_config().await?;
    let service = runtime.skill_catalog_for_agent(agent).await?;
    let guard = runtime.project_skill_read_guard(agent).await;
    let catalog = service
        .discover(&request, &config.skills, CancellationToken::new())
        .await?;
    drop(guard);
    let mut projected = service.project(&catalog, &request, &config.skills);
    if let Some(project_id) = agent.summary.read().await.project_id {
        runtime
            .apply_project_skill_source_paths_for_agent(agent, project_id, &mut projected)
            .await;
    }
    runtime
        .sync_agent_skills_to_container(agent, &projected)
        .await?;
    Ok(Some(catalog))
}

/// 解析本 Thread 的协作工具可见性。
///
/// Review Thread 物理上不挂载协作工具组，因此审查会话无法 spawn、投递或关闭其它 Agent。
/// 普通 Thread 挂载真实的 PL collaboration 工具组：host 只持 runtime 的弱引用，父子关系、
/// Profile 能力与工作区处置都留在产品侧判定。
fn collaboration_availability(
    runtime: &Arc<AgentRuntime>,
    review_thread: bool,
) -> CollaborationAvailability {
    if review_thread {
        return CollaborationAvailability::Disabled;
    }
    CollaborationAvailability::Enabled(Arc::new(crate::thread_host::MaiThreadCollaboration::new(
        runtime,
    )))
}

/// 由 PL 的公共 provider 能力规划，装配独占于本 Thread 的搜索工具或托管声明。
fn search_binding(
    config: &MaiConfig,
    summary: &mai_protocol::AgentSummary,
) -> Result<MaiSearchBinding> {
    let route = match crate::thread_host::resolve_agent_route(&config.models, summary) {
        crate::thread_host::ProductRoute::Available(route) => route,
        crate::thread_host::ProductRoute::Unavailable { reason, .. } => {
            return Err(RuntimeError::InvalidInput(reason));
        }
    };
    let plans = crate::runtime_tool_settings::product_web_search_plans(config, &route)?;
    let binding = plans.build_thread(&config.web_search).map_err(|error| {
        RuntimeError::InvalidInput(format!("cannot bind web search for Thread: {error}"))
    })?;
    let visibility = match binding.visibility {
        pl_tool::search::ToolVisibilityConstraint::Additive
        | pl_tool::search::ToolVisibilityConstraint::Unavailable => SearchVisibility::Additive,
        pl_tool::search::ToolVisibilityConstraint::Exclusive => SearchVisibility::Exclusive,
    };
    Ok(MaiSearchBinding {
        hosted_tools: binding.hosted,
        registrations: binding.tools,
        visibility,
    })
}
