//! 产品 Agent 的容器工作区工具绑定。
//!
//! 一个 Thread 独占一套 `exec`/`write_stdin`/`read_file`/`list_files`/`apply_patch` 执行器：
//! 命令后端把进程映射到本 Agent 的容器，文件后端把容器内 POSIX 路径绑定到冻结后的
//! workspace 边界，命令输出则归档进内容寻址的 [`MaiResourceStore`]。
//!
//! 本模块只做产品事实到 PL 工具接口的装配：进程表、stdin、路径策略、输出截断与 artifact
//! 语义分别由 `turn::command`、`turn::workspace_file` 与 `thread_resources` 提供。

use std::path::PathBuf;
use std::sync::Arc;

use mai_protocol::{AgentId, AgentRole, AgentSummary};
use pl_core::tool::opaque::Registration;
use pl_protocol::{AgentWorkspaceAssignmentSnapshot, AgentWorkspaceMode};
use pl_tool::exec::CommandAccess;
use pl_tool::workspace::{AgentWorkspace, ToolWorkspace, WorkspaceBoundary, WorkspaceMutability};

use crate::thread_resources::{MaiResourceStore, thread_resources_root};
use crate::turn::command::MaiCommandBackend;
use crate::turn::container::MaiContainerBackend;
use crate::turn::tool_sets::{
    ThreadWorkspaceTools, command_registrations, workspace_file_registrations,
};
use crate::turn::workspace_file::MaiWorkspaceFileBackend;
use crate::{AgentRuntime, Result, RuntimeError};

/// 没有绑定项目仓库的 Agent 容器内工作区根；这是容器 workspace volume 的挂载点。
pub(crate) const AGENT_CONTAINER_WORKSPACE_ROOT: &str = "/workspace";

/// 一个 Thread 独占的容器工作区工具集合。
pub(crate) struct MaiThreadWorkspaceTools {
    workspace: ToolWorkspace,
    commands: Arc<MaiCommandBackend>,
    archive: Arc<MaiResourceStore>,
    files: Arc<MaiWorkspaceFileBackend>,
    access: CommandAccess,
}

impl MaiThreadWorkspaceTools {
    /// 按产品 Agent 事实构造该 Thread 唯一的工作区工具实例。
    ///
    /// 调用方必须先确保 Agent 容器已经驻留：命令与文件后端都在容器内解析路径，容器不存在
    /// 时装配会以显式错误失败，而不是发布一个指向不存在的物理后端的 Thread。
    pub(crate) fn new(
        runtime: Arc<AgentRuntime>,
        agent_id: AgentId,
        summary: &AgentSummary,
    ) -> Self {
        let workspace = agent_workspace(summary);
        let access = match workspace.boundary() {
            WorkspaceBoundary::Confined => CommandAccess::WorkspaceOnly,
            WorkspaceBoundary::HostPermitted => CommandAccess::HostGranted,
        };
        let root = workspace.root().to_path_buf();
        let container = Arc::new(MaiContainerBackend::new(Arc::clone(&runtime), agent_id));
        let files = Arc::new(MaiWorkspaceFileBackend::new(container, &workspace));
        let commands = Arc::new(MaiCommandBackend::new(
            Arc::clone(&runtime),
            agent_id,
            root.clone(),
        ));
        let archive = Arc::new(MaiResourceStore::new(thread_resources_root(
            &runtime.artifact_files_root,
        )));
        Self {
            workspace: ToolWorkspace::new(workspace),
            commands,
            archive,
            files,
            access,
        }
    }
}

impl std::fmt::Debug for MaiThreadWorkspaceTools {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MaiThreadWorkspaceTools")
            .field("workspace", &self.workspace)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

impl ThreadWorkspaceTools for MaiThreadWorkspaceTools {
    fn tool_workspace(&self) -> &ToolWorkspace {
        &self.workspace
    }

    fn command_registrations(&self) -> Result<Vec<Registration>> {
        command_registrations(
            Arc::clone(&self.commands),
            Arc::clone(&self.archive),
            self.access,
        )
    }

    fn workspace_file_registrations(&self) -> Result<Vec<Registration>> {
        workspace_file_registrations(Arc::clone(&self.files), self.workspace.clone())
    }
}

/// 依产品 Agent 事实解析容器工作区边界。
///
/// 新 PL Thread 不再接收会话级 workspace receipt，因此边界只来自产品事实：项目 Agent 以
/// 容器内 `/workspace/repo` 为根，其余 Agent 以容器 workspace 挂载点 `/workspace` 为根。
/// 根 Agent 的受控范围固定在项目根内；由产品创建的 child 按角色获得 Profile 语义：
/// `executor` 是受写目录约束的 `directory` 工作区，其余角色是 `unrestricted` 工作区。
/// Project Review Thread 无论角色如何都固定为只读受限工作区，审查期间不得改动仓库。
pub(crate) fn agent_workspace(summary: &AgentSummary) -> AgentWorkspace {
    let root: PathBuf = if summary.project_id.is_some() {
        crate::projects::workspace::AGENT_WORKSPACE_REPO_PATH.into()
    } else {
        AGENT_CONTAINER_WORKSPACE_ROOT.into()
    };
    if summary.review_run_id.is_some() {
        return AgentWorkspace::confined(root, WorkspaceMutability::ReadOnly);
    }
    // 协作 child 在创建时冻结了自己的工作区收据；只要有收据，它就是这个 Thread 的唯一权威，
    // 不再按角色重新猜测边界。
    if let Some(assignment) = &summary.workspace {
        return workspace_from_assignment(assignment);
    }
    if summary.parent_id.is_none() {
        return AgentWorkspace::confined(root, WorkspaceMutability::ReadWrite);
    }
    match summary.role.unwrap_or_default() {
        // `None` 表示整个项目可写；executor 的具体 writablePaths 由父 Agent 在创建 child 时给出，
        // 产品当前没有持久化该 receipt，因此这里只能表达“项目内可写”。
        AgentRole::Executor => AgentWorkspace::directory(root, None),
        AgentRole::Planner | AgentRole::Explorer | AgentRole::Reviewer => {
            AgentWorkspace::local(root)
        }
    }
}

/// 把创建时冻结的工作区收据投影成 PL 的 canonical [`AgentWorkspace`]。
///
/// `Directory` 保留项目相对写策略，`Unrestricted` 不额外限制目录，`Worktree` 表达物理隔离的
/// worktree；mai 的协作 spawn 只冻结前两种，`Worktree` 只会在恢复一份外部写入的产品事实时出现。
fn workspace_from_assignment(assignment: &AgentWorkspaceAssignmentSnapshot) -> AgentWorkspace {
    let root = PathBuf::from(&assignment.root);
    let project_root = PathBuf::from(&assignment.project_root);
    match assignment.mode {
        AgentWorkspaceMode::Directory => AgentWorkspace::directory(
            project_root,
            assignment
                .writable_paths
                .as_ref()
                .map(|paths| paths.iter().map(PathBuf::from).collect()),
        ),
        AgentWorkspaceMode::Unrestricted => AgentWorkspace::local(root),
        AgentWorkspaceMode::Worktree => AgentWorkspace::worktree(project_root, root),
    }
}

/// 冻结一个由父 Agent 创建的 child 的工作区收据。
///
/// 语义与旧 `profile_catalog::workspace_assignment` 一致：`executor` 是 `directory` 工作区，
/// 可以由 spawn 参数收紧写入目录；其它角色是 `unrestricted` 工作区，不接受写目录参数。
/// `writable_paths` 是项目相对路径，这里绝对化到容器内项目根，并拒绝绝对路径与 `..` 逃逸。
pub(crate) fn child_workspace_assignment(
    parent: &AgentSummary,
    role: AgentRole,
    writable_paths: Option<&[String]>,
) -> Result<AgentWorkspaceAssignmentSnapshot> {
    let project_root = if parent.project_id.is_some() {
        crate::projects::workspace::AGENT_WORKSPACE_REPO_PATH.to_string()
    } else {
        AGENT_CONTAINER_WORKSPACE_ROOT.to_string()
    };
    let mode = match role {
        AgentRole::Executor => AgentWorkspaceMode::Directory,
        AgentRole::Planner | AgentRole::Explorer | AgentRole::Reviewer => {
            AgentWorkspaceMode::Unrestricted
        }
    };
    let writable_paths = match mode {
        AgentWorkspaceMode::Directory => match writable_paths {
            Some(paths) => Some(normalize_writable_paths(&project_root, paths)?),
            None => None,
        },
        AgentWorkspaceMode::Unrestricted | AgentWorkspaceMode::Worktree => {
            if writable_paths.is_some_and(|paths| !paths.is_empty()) {
                return Err(RuntimeError::InvalidInput(format!(
                    "role `{role}` uses an unrestricted workspace and does not accept writable paths"
                )));
            }
            None
        }
    };
    Ok(AgentWorkspaceAssignmentSnapshot {
        mode,
        project_root: project_root.clone(),
        root: project_root,
        writable_paths,
        worktree: None,
    })
}

/// 把项目相对写目录绝对化到容器内项目根；空项被跳过，逃逸路径显式报错。
fn normalize_writable_paths(project_root: &str, writable_paths: &[String]) -> Result<Vec<String>> {
    let project_root = project_root.trim_end_matches('/');
    let mut normalized = Vec::new();
    for path in writable_paths {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            continue;
        }
        let candidate = std::path::Path::new(trimmed);
        if candidate.is_absolute()
            || candidate
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(RuntimeError::InvalidInput(format!(
                "child writable path `{trimmed}` must be project-relative without `..`"
            )));
        }
        normalized.push(format!(
            "{project_root}/{}",
            trimmed.trim_start_matches("./").trim_start_matches('/')
        ));
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn directory_assignment_keeps_project_writable_paths() {
        let assignment = AgentWorkspaceAssignmentSnapshot {
            mode: AgentWorkspaceMode::Directory,
            project_root: "/workspace/repo".to_string(),
            root: "/workspace/repo".to_string(),
            writable_paths: Some(vec!["/workspace/repo/src".to_string()]),
            worktree: None,
        };

        let workspace = workspace_from_assignment(&assignment);

        assert_eq!(workspace.root(), std::path::Path::new("/workspace/repo"));
        assert_eq!(workspace.boundary(), WorkspaceBoundary::HostPermitted);
        assert_eq!(
            workspace.project_writable_paths(),
            Some([PathBuf::from("/workspace/repo/src")].as_slice())
        );
    }

    #[test]
    fn unrestricted_assignment_is_host_permitted_without_write_limits() {
        let assignment = AgentWorkspaceAssignmentSnapshot {
            mode: AgentWorkspaceMode::Unrestricted,
            project_root: "/workspace/repo".to_string(),
            root: "/workspace/repo".to_string(),
            writable_paths: None,
            worktree: None,
        };

        let workspace = workspace_from_assignment(&assignment);

        assert_eq!(workspace.boundary(), WorkspaceBoundary::HostPermitted);
        assert_eq!(workspace.project_writable_paths(), None);
    }

    #[test]
    fn writable_paths_are_absolutized_against_the_project_root() {
        let normalized = normalize_writable_paths(
            "/workspace/repo",
            &["src".to_string(), "./tests".to_string(), " ".to_string()],
        )
        .expect("normalize");

        assert_eq!(
            normalized,
            vec![
                "/workspace/repo/src".to_string(),
                "/workspace/repo/tests".to_string(),
            ]
        );
    }

    #[test]
    fn absolute_or_escaping_writable_paths_are_rejected() {
        for path in ["/etc", "../outside", "src/../../outside"] {
            assert!(
                normalize_writable_paths("/workspace/repo", &[path.to_string()]).is_err(),
                "path `{path}` must be rejected"
            );
        }
    }
}
