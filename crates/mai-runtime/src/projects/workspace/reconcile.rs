use std::collections::HashSet;
use std::path::{Path, PathBuf};

use mai_protocol::{AgentId, AgentSummary, ProjectId, ProjectSummary};

use super::paths::project_paths;
use crate::Result;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceReconcileReport {
    pub(crate) orphan_clones_removed: Vec<AgentId>,
    pub(crate) orphan_clone_removal_failed: Vec<PathBuf>,
    pub(crate) orphan_project_dirs_archived: Vec<ProjectId>,
    pub(crate) legacy_worktree_dirs_archived: Vec<ProjectId>,
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
        .collect::<HashSet<_>>();

    for project in live_projects {
        let paths = project_paths(projects_root, project.id);
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
                if !live_agent_projects.contains(&(agent_id, project.id)) {
                    let path = entry.path();
                    match std::fs::remove_dir_all(&path) {
                        Ok(()) => report.orphan_clones_removed.push(agent_id),
                        Err(err) => {
                            tracing::warn!(
                                path = %path.display(),
                                "failed to remove orphan project clone during startup reconcile: {err}"
                            );
                            report.orphan_clone_removal_failed.push(path);
                        }
                    }
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

    report.orphan_clones_removed.sort();
    report.orphan_clone_removal_failed.sort();
    report.orphan_project_dirs_archived.sort();
    report.legacy_worktree_dirs_archived.sort();
    report.invalid_clone_dirs.sort();
    Ok(report)
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
