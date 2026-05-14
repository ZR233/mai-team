use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerPaths {
    pub(crate) data_dir: PathBuf,
    pub(crate) cache_dir: PathBuf,
    pub(crate) projects_root: PathBuf,
    pub(crate) artifact_files_root: PathBuf,
    pub(crate) artifact_index_root: PathBuf,
    pub(crate) system_skills_root: PathBuf,
    pub(crate) system_agents_root: PathBuf,
}

impl ServerPaths {
    pub(crate) fn from_data_path(current_dir: &Path, data_path: Option<PathBuf>) -> Self {
        let data_dir = data_path.unwrap_or_else(|| current_dir.join(".mai-team"));
        Self {
            cache_dir: data_dir.join("cache"),
            projects_root: data_dir.join("projects"),
            artifact_files_root: data_dir.join("artifacts").join("files"),
            artifact_index_root: data_dir.join("artifacts").join("index"),
            system_skills_root: data_dir.join("system-skills"),
            system_agents_root: data_dir.join("system-agents"),
            data_dir,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::ServerPaths;

    #[test]
    fn uses_default_data_layout() {
        let current_dir = PathBuf::from("/workspace/mai-team");

        let paths = ServerPaths::from_data_path(&current_dir, None);

        assert_eq!(paths.data_dir, current_dir.join(".mai-team"));
        assert_eq!(paths.cache_dir, current_dir.join(".mai-team/cache"));
        assert_eq!(paths.projects_root, current_dir.join(".mai-team/projects"));
        assert_eq!(
            paths.artifact_files_root,
            current_dir.join(".mai-team/artifacts/files")
        );
        assert_eq!(
            paths.artifact_index_root,
            current_dir.join(".mai-team/artifacts/index")
        );
        assert_eq!(
            paths.system_skills_root,
            current_dir.join(".mai-team/system-skills")
        );
        assert_eq!(
            paths.system_agents_root,
            current_dir.join(".mai-team/system-agents")
        );
    }

    #[test]
    fn uses_cli_data_path() {
        let current_dir = PathBuf::from("/workspace/mai-team");
        let data_dir = PathBuf::from("/tmp/mai-data");

        let paths = ServerPaths::from_data_path(&current_dir, Some(data_dir.clone()));

        assert_eq!(paths.data_dir, data_dir);
        assert_eq!(paths.cache_dir, PathBuf::from("/tmp/mai-data/cache"));
    }
}
