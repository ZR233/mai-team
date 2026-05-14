use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use mai_protocol::{
    AgentId, AgentStatus, AgentSummary, ProjectCloneStatus, ProjectId, ProjectStatus,
    ProjectSummary,
};

use super::paths::{agent_clone_path, project_paths};
use crate::Result;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceReconcileReport {
    pub(crate) orphan_clones_removed: Vec<AgentId>,
    pub(crate) orphan_project_dirs_archived: Vec<ProjectId>,
    pub(crate) legacy_worktree_dirs_archived: Vec<ProjectId>,
    pub(crate) missing_repo_caches: Vec<ProjectId>,
    pub(crate) missing_agent_clones: Vec<AgentId>,
    pub(crate) invalid_clone_dirs: Vec<PathBuf>,
}

pub(crate) fn reconcile_project_workspaces(
    projects_root: &Path,
    live_projects: &[ProjectSummary],
    live_agents: &[AgentSummary],
) -> Result<WorkspaceReconcileReport> {
    let mut report = WorkspaceReconcileReport::default();
    let live_project_ids = live_projects
        .iter()
        .map(|project| project.id)
        .collect::<HashSet<_>>();
    let live_agent_projects = live_agents
        .iter()
        .filter_map(|agent| agent.project_id.map(|project_id| (agent.id, project_id)))
        .collect::<HashMap<_, _>>();

    for project in live_projects {
        let paths = project_paths(projects_root, project.id);
        if project_workspace_should_exist(project) && !paths.repo_cache_path.exists() {
            report.missing_repo_caches.push(project.id);
        }
        let legacy_worktrees = paths.project_dir.join("worktrees");
        if legacy_worktrees.exists() {
            std::fs::rename(
                &legacy_worktrees,
                next_legacy_worktree_archive_path(&paths.project_dir),
            )?;
            report.legacy_worktree_dirs_archived.push(project.id);
        }
        if paths.clones_dir.exists() {
            for entry in std::fs::read_dir(&paths.clones_dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let clone_owner = entry.file_name().to_string_lossy().into_owned();
                let Ok(agent_id) = AgentId::parse_str(&clone_owner) else {
                    report.invalid_clone_dirs.push(entry.path());
                    continue;
                };
                if live_agent_projects.get(&agent_id) != Some(&project.id) {
                    std::fs::remove_dir_all(entry.path())?;
                    report.orphan_clones_removed.push(agent_id);
                }
            }
        }
    }
    if projects_root.exists() {
        for entry in std::fs::read_dir(projects_root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let project_name = entry.file_name().to_string_lossy().into_owned();
            let Ok(project_id) = ProjectId::parse_str(&project_name) else {
                continue;
            };
            if live_project_ids.contains(&project_id) {
                continue;
            }
            let archive_path = next_orphan_project_archive_path(projects_root, project_id);
            if let Some(parent) = archive_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(entry.path(), archive_path)?;
            report.orphan_project_dirs_archived.push(project_id);
        }
    }

    for agent in live_agents {
        let Some(project_id) = agent.project_id else {
            continue;
        };
        if !live_project_ids.contains(&project_id) || !agent_workspace_should_exist(agent) {
            continue;
        }
        let Some(project) = live_projects
            .iter()
            .find(|project| project.id == project_id)
        else {
            continue;
        };
        if project_workspace_should_exist(project)
            && !agent_clone_path(projects_root, project_id, agent.id).exists()
        {
            report.missing_agent_clones.push(agent.id);
        }
    }

    report.orphan_clones_removed.sort();
    report.orphan_project_dirs_archived.sort();
    report.legacy_worktree_dirs_archived.sort();
    report.missing_repo_caches.sort();
    report.missing_agent_clones.sort();
    report.invalid_clone_dirs.sort();
    Ok(report)
}

fn project_workspace_should_exist(project: &ProjectSummary) -> bool {
    match (&project.status, &project.clone_status) {
        (ProjectStatus::Ready, ProjectCloneStatus::Ready) => true,
        (ProjectStatus::Creating, ProjectCloneStatus::Pending)
        | (ProjectStatus::Creating, ProjectCloneStatus::Cloning)
        | (ProjectStatus::Creating, ProjectCloneStatus::Ready)
        | (ProjectStatus::Creating, ProjectCloneStatus::Failed)
        | (ProjectStatus::Failed, ProjectCloneStatus::Pending)
        | (ProjectStatus::Failed, ProjectCloneStatus::Cloning)
        | (ProjectStatus::Failed, ProjectCloneStatus::Ready)
        | (ProjectStatus::Failed, ProjectCloneStatus::Failed)
        | (ProjectStatus::Ready, ProjectCloneStatus::Pending)
        | (ProjectStatus::Ready, ProjectCloneStatus::Cloning)
        | (ProjectStatus::Ready, ProjectCloneStatus::Failed)
        | (ProjectStatus::Deleting, ProjectCloneStatus::Pending)
        | (ProjectStatus::Deleting, ProjectCloneStatus::Cloning)
        | (ProjectStatus::Deleting, ProjectCloneStatus::Ready)
        | (ProjectStatus::Deleting, ProjectCloneStatus::Failed) => false,
    }
}

fn next_orphan_project_archive_path(projects_root: &Path, project_id: ProjectId) -> PathBuf {
    let orphaned_root = projects_root.join("orphaned");
    let project_name = project_id.to_string();
    let mut index = 0;
    loop {
        let name = if index == 0 {
            project_name.clone()
        } else {
            format!("{project_name}-{index}")
        };
        let candidate = orphaned_root.join(name);
        if !candidate.exists() {
            return candidate;
        }
        index += 1;
    }
}

fn next_legacy_worktree_archive_path(project_dir: &Path) -> PathBuf {
    let mut index = 0;
    loop {
        let name = if index == 0 {
            "legacy-worktrees".to_string()
        } else {
            format!("legacy-worktrees-{index}")
        };
        let candidate = project_dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
        index += 1;
    }
}

fn agent_workspace_should_exist(agent: &AgentSummary) -> bool {
    match agent.status {
        AgentStatus::Created
        | AgentStatus::StartingContainer
        | AgentStatus::Idle
        | AgentStatus::RunningTurn
        | AgentStatus::WaitingTool
        | AgentStatus::Completed
        | AgentStatus::Failed
        | AgentStatus::Cancelled
        | AgentStatus::DeletingContainer => true,
        AgentStatus::Deleted => false,
    }
}
