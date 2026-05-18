use std::path::{Path, PathBuf};

use mai_protocol::{AgentId, ProjectId};

pub(crate) const PROJECT_REPO_CACHE_DIR: &str = "repo.git";
pub(crate) const PROJECT_CLONES_DIR: &str = "clones";
pub(crate) const PROJECT_TMP_DIR: &str = "tmp";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ProjectWorkspacePaths {
    pub(crate) project_dir: PathBuf,
    pub(crate) repo_cache_path: PathBuf,
    pub(crate) clones_dir: PathBuf,
    pub(crate) tmp_dir: PathBuf,
}

pub(crate) fn project_dir(projects_root: &Path, project_id: ProjectId) -> PathBuf {
    projects_root.join(project_id.to_string())
}

pub(crate) fn project_paths(projects_root: &Path, project_id: ProjectId) -> ProjectWorkspacePaths {
    let project_dir = project_dir(projects_root, project_id);
    ProjectWorkspacePaths {
        repo_cache_path: project_dir.join(PROJECT_REPO_CACHE_DIR),
        clones_dir: project_dir.join(PROJECT_CLONES_DIR),
        tmp_dir: project_dir.join(PROJECT_TMP_DIR),
        project_dir,
    }
}

#[cfg(test)]
pub(crate) fn project_repo_cache_path(projects_root: &Path, project_id: ProjectId) -> PathBuf {
    project_paths(projects_root, project_id).repo_cache_path
}

pub(crate) fn agent_clone_path(
    projects_root: &Path,
    project_id: ProjectId,
    agent_id: AgentId,
) -> PathBuf {
    project_paths(projects_root, project_id)
        .clones_dir
        .join(agent_id.to_string())
        .join("repo")
}

#[cfg(test)]
pub(crate) fn project_tmp_path(projects_root: &Path, project_id: ProjectId) -> PathBuf {
    project_paths(projects_root, project_id).tmp_dir
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn project_workspace_paths_use_repo_cache_and_agent_clones() {
        let root = PathBuf::from("/data/.mai-team/projects");
        let project_id = uuid::Uuid::nil();
        let agent_id = uuid::Uuid::nil();

        let paths = project_paths(&root, project_id);

        assert_eq!(
            paths,
            ProjectWorkspacePaths {
                project_dir: root.join(project_id.to_string()),
                repo_cache_path: root.join(project_id.to_string()).join("repo.git"),
                clones_dir: root.join(project_id.to_string()).join("clones"),
                tmp_dir: root.join(project_id.to_string()).join("tmp"),
            }
        );
        assert_eq!(
            agent_clone_path(&root, project_id, agent_id),
            root.join(project_id.to_string())
                .join("clones")
                .join(agent_id.to_string())
                .join("repo")
        );
    }
}
