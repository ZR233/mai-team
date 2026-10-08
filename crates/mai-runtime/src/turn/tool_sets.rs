//! mai 产品 Thread 的工具目录装配构件。
//!
//! 该模块把 mai 已经解析好的物理后端与授权身份转换成
//! `pl_core::tool::opaque::Registration`，并把 provider 托管的声明汇总成
//! `pl_model::runtime::HostedTool`。工具实现全部来自 pl-tool；这里只做装配与声明编码。
//!
//! 所有权约定：每个 `Registration` 都持有独占的执行器，装配成功后只能转移给唯一一个
//! PL Thread。调用方不得缓存、克隆复用，或把同一批注册项安装到第二个 Thread。

use std::sync::Arc;

use pl_core::{context::OpaquePayload, tool::opaque::Registration};
use pl_model::runtime::HostedTool;
use pl_tool::collaboration::thread::{AgentControlHost, AgentControlKind, ThreadAgentControl};
use pl_tool::skill::{FrozenSkillCatalog, ThreadSkillKind, ThreadSkillTool};
use pl_tool::thread_catalog::ThreadBuiltin;
use pl_tool::workspace::ToolWorkspace;

use crate::{Result, RuntimeError};

/// 一次 Thread 装配产出的完整工具目录。
#[derive(Debug, Default)]
pub(crate) struct ThreadToolCatalog {
    hosted_tools: Vec<HostedTool>,
    registrations: Vec<Registration>,
}

impl ThreadToolCatalog {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 追加 provider 托管声明；这些声明不会产生本地执行器。
    pub(crate) fn push_hosted(&mut self, tool: HostedTool) {
        self.hosted_tools.push(tool);
    }

    /// 批量追加本地注册项。
    pub(crate) fn extend(&mut self, registrations: impl IntoIterator<Item = Registration>) {
        self.registrations.extend(registrations);
    }

    /// 拆分为 `ThreadSpec.hosted_tools` 与 `ThreadSpec.registrations`。
    pub(crate) fn into_parts(self) -> (Vec<HostedTool>, Vec<Registration>) {
        (self.hosted_tools, self.registrations)
    }
}

/// mai 配置层已经规划完成的 Web Search 绑定。
///
/// provider 与模型能力判定依赖 mai 的模型配置，因此规划结果由 mai 配置层给出；本模块只
/// 负责把它们并入 Thread 工具目录。
#[derive(Debug, Default)]
pub(crate) struct MaiSearchBinding {
    /// provider 托管声明；不会产生本地执行器。
    pub(crate) hosted_tools: Vec<HostedTool>,
    /// mai 侧 standalone 搜索工具注册项；没有独立搜索后端时为空。
    pub(crate) registrations: Vec<Registration>,
}

/// 容器/工作区后端迁移方必须实现的最小稳定接口。
///
/// mai 的执行与文件操作都发生在 agent 容器内，因此这些注册项必须由容器后端提供：
/// 实现方用 pl-tool 的 `ThreadExecTool` 绑定命令后端与输出归档，用
/// `ThreadWorkspaceFileTool` 绑定工作区文件后端，再原样返回注册项。本模块不构造
/// 任何容器传输、进程表或路径策略。
pub(crate) trait ThreadWorkspaceTools: Send + Sync + std::fmt::Debug + 'static {
    /// 该 Thread 的统一工作区授权身份；Git 与文件注册项使用同一身份。
    fn tool_workspace(&self) -> &ToolWorkspace;

    /// 产出互配的 `exec` 与 `write_stdin` 注册项。
    fn command_registrations(&self) -> Result<Vec<Registration>>;

    /// 产出 `read_file`、`list_files` 与 `apply_patch` 注册项。
    fn workspace_file_registrations(&self) -> Result<Vec<Registration>>;
}

/// mai 协作协调器必须实现的最小稳定接口。
///
/// 实现方用 pl-tool 的 `collaboration_registrations` 绑定自己的 `AgentControlHost`
/// 即可；父子关系、角色许可与 workspace disposition 判定都留在该 host 内。
pub(crate) trait ThreadCollaborationTools: Send + Sync + std::fmt::Debug + 'static {
    /// 产出 `spawn_agent`、`send_message`、`list_agents`、`interrupt_agent` 与
    /// `close_agent` 注册项。
    fn registrations(&self) -> Result<Vec<Registration>>;
}

/// 用 pl-tool 的 `ThreadExecTool` 构造 `exec`/`write_stdin` 注册项。
///
/// `archive` 必须把完整输出落成可读取的资源引用；缓存或宿主路径不属于本函数。
pub(crate) fn command_registrations<B, A>(
    backend: Arc<B>,
    archive: Arc<A>,
    access: pl_tool::exec::CommandAccess,
) -> Result<Vec<Registration>>
where
    B: pl_tool::command::CommandBackend + 'static,
    A: pl_tool::exec::CommandOutputArchive + 'static,
{
    let exec = declaration(ThreadBuiltin::Exec)?;
    let write_stdin = declaration(ThreadBuiltin::WriteStdin)?;
    pl_tool::exec::ThreadExecTool::new(backend, archive, access)
        .registrations(exec, write_stdin)
        .map_err(|error| registration_error("exec", error))
}

/// 用 pl-tool 的 `ThreadWorkspaceFileTool` 构造只读与 patch 文件工具。
pub(crate) fn workspace_file_registrations<B>(
    backend: Arc<B>,
    workspace: ToolWorkspace,
) -> Result<Vec<Registration>>
where
    B: pl_tool::workspace_file::WorkspaceFileBackend + 'static,
{
    pl_tool::workspace_file::WorkspaceFileToolKind::all()
        .iter()
        .copied()
        .map(|kind| {
            pl_tool::workspace_file::ThreadWorkspaceFileTool::new(
                kind,
                backend.clone(),
                workspace.clone(),
            )
            .registration(declaration(ThreadBuiltin::File(kind))?)
            .map_err(|error| registration_error(kind.name(), error))
        })
        .collect()
}

/// 用 pl-tool 的技能绑定冻结目录；只暴露读取，不注册本地写入管理。
pub(crate) fn skill_registrations(catalog: &Arc<FrozenSkillCatalog>) -> Result<Vec<Registration>> {
    [ThreadSkillKind::List, ThreadSkillKind::View]
        .into_iter()
        .map(|kind| {
            let declaration = model_declaration(&kind.declaration())?;
            ThreadSkillTool::new(catalog.clone(), kind)
                .registration(declaration)
                .map_err(|error| registration_error(skill_kind_name(kind), error))
        })
        .collect()
}

/// 用 pl-tool 的 `ThreadAgentControl` 构造完整的协作工具注册项。
pub(crate) fn collaboration_registrations<H>(host: Arc<H>) -> Result<Vec<Registration>>
where
    H: AgentControlHost + 'static,
{
    [
        AgentControlKind::Spawn,
        AgentControlKind::Send,
        AgentControlKind::List,
        AgentControlKind::Interrupt,
        AgentControlKind::Close,
    ]
    .into_iter()
    .map(|kind| {
        let declaration = model_declaration(&kind.declaration())?;
        ThreadAgentControl::new(host.clone(), kind)
            .registration(declaration)
            .map_err(|error| registration_error(agent_control_name(kind), error))
    })
    .collect()
}

/// PL 标准 Thread 内建工具：用户交互、待办、回合结束、工具发现、任务控制与笔记。
///
/// 这些实现全部来自 pl-tool，取代旧引擎隐式安装的 `builtin` 工具组。
pub(crate) fn standard_registrations() -> Result<Vec<Registration>> {
    let mut registrations = vec![
        registered(
            "request_user_input",
            pl_tool::ask_user::registration(declaration(ThreadBuiltin::AskUser)?),
        )?,
        registered(
            "finish_turn",
            pl_tool::finish_turn::registration(declaration(ThreadBuiltin::FinishTurn)?),
        )?,
        registered(
            "update_todo_list",
            pl_tool::todo::registration(declaration(ThreadBuiltin::Todo)?),
        )?,
        registered(
            "discover_tools",
            pl_tool::discovery::registration(declaration(ThreadBuiltin::Discover)?),
        )?,
        registered(
            "list_tool_tasks",
            pl_tool::task_control::ListTasksTool
                .registration(declaration(ThreadBuiltin::ListTasks)?),
        )?,
        registered(
            "sleep",
            Registration::new(
                "sleep".to_string(),
                declaration(ThreadBuiltin::Sleep)?,
                pl_tool::session::SleepTool,
            ),
        )?,
    ];
    for kind in [
        pl_tool::task_control::TaskControlKind::Wait,
        pl_tool::task_control::TaskControlKind::Query,
        pl_tool::task_control::TaskControlKind::Cancel,
    ] {
        registrations.push(registered(
            task_control_name(kind),
            pl_tool::task_control::TaskControlTool::new(kind)
                .registration(declaration(ThreadBuiltin::Task(kind))?),
        )?);
    }
    for kind in pl_tool::session_note::SessionNoteToolKind::all() {
        registrations.push(registered(
            kind.name(),
            pl_tool::session_note::registration(*kind, declaration(ThreadBuiltin::Note(*kind))?),
        )?);
    }
    Ok(registrations)
}

fn declaration(tool: ThreadBuiltin) -> Result<OpaquePayload> {
    model_declaration(&tool.declaration())
}

fn model_declaration(spec: &pl_protocol::ToolSpec) -> Result<OpaquePayload> {
    pl_model::runtime::thread_tool_declaration(spec).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "failed to encode Thread tool declaration `{}`: {error}",
            spec.name()
        ))
    })
}

fn registered(
    tool: &str,
    registration: std::result::Result<Registration, pl_core::tool::opaque::RegistryError>,
) -> Result<Registration> {
    registration.map_err(|error| registration_error(tool, error))
}

fn registration_error(tool: &str, error: pl_core::tool::opaque::RegistryError) -> RuntimeError {
    RuntimeError::InvalidInput(format!("failed to register Thread tool `{tool}`: {error}"))
}

fn skill_kind_name(kind: ThreadSkillKind) -> &'static str {
    match kind {
        ThreadSkillKind::List => "skills_list",
        ThreadSkillKind::View => "skill_view",
    }
}

fn agent_control_name(kind: AgentControlKind) -> &'static str {
    match kind {
        AgentControlKind::Spawn => "spawn_agent",
        AgentControlKind::Send => "send_message",
        AgentControlKind::List => "list_agents",
        AgentControlKind::Interrupt => "interrupt_agent",
        AgentControlKind::Close => "close_agent",
    }
}

fn task_control_name(kind: pl_tool::task_control::TaskControlKind) -> &'static str {
    match kind {
        pl_tool::task_control::TaskControlKind::Wait => "wait",
        pl_tool::task_control::TaskControlKind::Query => "get_tool_task",
        pl_tool::task_control::TaskControlKind::Cancel => "cancel_tool_task",
    }
}
