use std::fs;
use std::path::{Path, PathBuf};

use crate::constants::{SKILL_FILE, SKILL_PATH_PREFIX};

pub(crate) fn canonicalize_or_clone(path: impl AsRef<Path>) -> PathBuf {
    fs::canonicalize(path.as_ref()).unwrap_or_else(|_| path.as_ref().to_path_buf())
}

pub(crate) fn normalized_skill_path(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    let normalized = raw.strip_prefix(SKILL_PATH_PREFIX).unwrap_or(&raw);
    canonicalize_or_clone(Path::new(normalized))
}

pub(crate) fn looks_like_path(value: &str) -> bool {
    value.starts_with(SKILL_PATH_PREFIX)
        || value.contains('/')
        || value.contains('\\')
        || value.ends_with(SKILL_FILE)
}
