use std::env;
use std::path::{Path, PathBuf};

use anyhow::Result;

pub(crate) fn data_dir_path(cli_data_path: Option<PathBuf>) -> Result<PathBuf> {
    Ok(match cli_data_path {
        Some(path) => path,
        None => env::current_dir()?.join(".mai-team"),
    })
}

pub(crate) fn cache_dir_path(data_dir: &Path) -> PathBuf {
    data_dir.join("cache")
}

pub(crate) fn artifact_files_root(data_dir: &Path) -> PathBuf {
    data_dir.join("artifacts").join("files")
}

pub(crate) fn artifact_index_root(data_dir: &Path) -> PathBuf {
    data_dir.join("artifacts").join("index")
}

#[cfg(test)]
pub(crate) fn data_dir_path_with(current_dir: &Path, cli_data_path: Option<PathBuf>) -> PathBuf {
    cli_data_path.unwrap_or_else(|| current_dir.join(".mai-team"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn runtime_storage_paths_use_cli_data_path() {
        let dir = tempdir().expect("tempdir");
        let data_dir = dir.path().join("data-root");

        assert_eq!(
            data_dir_path_with(dir.path(), Some(data_dir.clone())),
            data_dir
        );
        assert_eq!(cache_dir_path(&data_dir), data_dir.join("cache"));
    }
}
