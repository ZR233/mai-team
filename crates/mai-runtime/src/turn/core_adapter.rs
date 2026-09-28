//! mai 产品 Thread 的工具装配入口。
//!
//! 该模块把 mai 已经解析好的 Agent、工作区后端与配置结果装配成 `ThreadSpec` 需要的
//! `Vec<pl_model::runtime::HostedTool>` 与 `Vec<pl_core::tool::opaque::Registration>`。
//! 模型路由、provider 能力判定与 Web Search 规划仍由 mai 自己负责；这里不读取配置，
//! 也不重新实现任何 pl-tool 已经提供的工具。

use std::sync::Arc;

use mai_protocol::AgentId;
use pl_core::tool::opaque::Registration;
use pl_tool::skill::FrozenSkillCatalog;

use crate::state::AgentRecord;
use crate::tools::git::NativeGitToolRuntime;
use crate::turn::tool_sets::{
    MaiSearchBinding, SearchVisibility, ThreadCollaborationTools, ThreadToolCatalog,
    ThreadWorkspaceTools, skill_registrations, standard_registrations,
};
use crate::{AgentRuntime, Result};

/// 协作工具是否对本 Thread 可见。
pub(crate) enum CollaborationAvailability {
    /// 挂载协作工具组；父子关系与角色许可由 host 实现判定。
    Enabled(Arc<dyn ThreadCollaborationTools>),
    /// 不挂载协作工具组。
    Disabled,
}

/// mai 侧一次 Thread 工具装配的全部输入。
///
/// `workspace`、`git` 与 `mcp` 的物理资源由各自迁移方构造；`search` 是 mai 配置层已经
/// 规划好的搜索绑定。装配产物中的每个注册项只能转移给一个 Thread。
pub(crate) struct MaiThreadToolContext {
    pub(crate) runtime: Arc<AgentRuntime>,
    pub(crate) agent: Arc<AgentRecord>,
    pub(crate) agent_id: AgentId,
    pub(crate) workspace: Arc<dyn ThreadWorkspaceTools>,
    pub(crate) git: Option<NativeGitToolRuntime>,
    pub(crate) skill_catalog: Option<Arc<FrozenSkillCatalog>>,
    pub(crate) mcp: Vec<Registration>,
    pub(crate) collaboration: CollaborationAvailability,
    pub(crate) search: MaiSearchBinding,
}

/// 装配一个 Thread 的完整工具目录。
///
/// `SearchVisibility::Exclusive` 时只保留搜索工具与 provider 托管声明，其它本地工具组
/// 与协作工具都不挂载；这是旧引擎“卸载其余工具组”语义的等价实现。
pub(crate) async fn assemble_thread_tools(ctx: MaiThreadToolContext) -> Result<ThreadToolCatalog> {
    let MaiThreadToolContext {
        runtime,
        agent,
        agent_id,
        workspace,
        git,
        skill_catalog,
        mcp,
        collaboration,
        search,
    } = ctx;
    let MaiSearchBinding {
        hosted_tools,
        registrations: search_registrations,
        visibility,
    } = search;
    let mut catalog = ThreadToolCatalog::new();
    for tool in hosted_tools {
        catalog.push_hosted(tool);
    }
    catalog.extend(search_registrations);
    if visibility == SearchVisibility::Exclusive {
        return Ok(catalog);
    }

    catalog.extend(workspace.command_registrations()?);
    catalog.extend(workspace.workspace_file_registrations()?);
    if let Some(git) = git {
        catalog.extend(git.registrations(workspace.tool_workspace().authorization())?);
    }
    if let Some(skill_catalog) = skill_catalog {
        catalog.extend(skill_registrations(&skill_catalog)?);
    }
    catalog.extend(mcp);
    let summary = agent.summary.read().await.clone();
    let product_tools = super::product_tools::MaiProductTools::new(runtime, agent, agent_id)
        .registrations(&summary)?;
    catalog.extend(product_tools);
    catalog.extend(standard_registrations()?);
    match collaboration {
        CollaborationAvailability::Enabled(host) => catalog.extend(host.registrations()?),
        CollaborationAvailability::Disabled => {}
    }
    Ok(catalog)
}
