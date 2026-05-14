use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mai_protocol::{ProjectId, SkillScope, SkillsListResponse};
use mai_skills::SkillsManager;
use mai_store::ConfigStore;
use tokio::sync::RwLock;

use crate::projects::mcp::PROJECT_WORKSPACE_PATH;
use crate::{Result, RuntimeError};

pub(crate) const PROJECT_SKILLS_CACHE_DIR: &str = "project-skills";

const PROJECT_SKILL_CANDIDATE_DIRS: [(&str, &str); 3] = [
    (".claude/skills", "claude"),
    (".agents/skills", "agents"),
    ("skills", "skills"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectSkillSourceDir {
    pub(crate) cache_name: String,
    pub(crate) container_path: String,
    pub(crate) host_path: Option<PathBuf>,
}

pub(crate) fn cache_dir(cache_root: &Path, project_id: ProjectId) -> PathBuf {
    cache_root
        .join(PROJECT_SKILLS_CACHE_DIR)
        .join(project_id.to_string())
}

pub(crate) fn roots(cache_dir: &Path) -> Vec<(PathBuf, SkillScope)> {
    PROJECT_SKILL_CANDIDATE_DIRS
        .iter()
        .map(|(_, cache_name)| (cache_dir.join(cache_name), SkillScope::Project))
        .collect()
}

pub(crate) fn roots_for_project(
    cache_root: &Path,
    project_id: ProjectId,
) -> Vec<(PathBuf, SkillScope)> {
    roots(&cache_dir(cache_root, project_id))
}

pub(crate) async fn list_from_cache(
    store: &ConfigStore,
    cache_root: &Path,
    lock: &Arc<RwLock<()>>,
    project_id: ProjectId,
) -> Result<SkillsListResponse> {
    let _guard = lock.read().await;
    let config = store.load_skills_config().await?;
    let mut response =
        SkillsManager::with_roots(roots_for_project(cache_root, project_id)).list(&config)?;
    apply_project_source_paths(cache_root, project_id, &mut response);
    Ok(response)
}

pub(crate) fn detect_existing_dirs_in_host_repo(repo_path: &Path) -> Vec<ProjectSkillSourceDir> {
    PROJECT_SKILL_CANDIDATE_DIRS
        .iter()
        .filter_map(|(relative, cache_name)| {
            let host_path = repo_path.join(relative);
            host_path.is_dir().then(|| ProjectSkillSourceDir {
                cache_name: (*cache_name).to_string(),
                container_path: format!("{PROJECT_WORKSPACE_PATH}/{relative}"),
                host_path: Some(host_path),
            })
        })
        .collect()
}

pub(crate) async fn refresh_cache(
    cache_root: &Path,
    lock: &Arc<RwLock<()>>,
    project_id: ProjectId,
    sources: &[ProjectSkillSourceDir],
) -> Result<()> {
    let _guard = lock.write().await;
    let cache_dir = cache_dir(cache_root, project_id);
    if cache_dir.exists() {
        fs::remove_dir_all(&cache_dir)?;
    }
    fs::create_dir_all(&cache_dir)?;
    for project_source in sources {
        let target = cache_dir.join(&project_source.cache_name);
        let host_source = project_source.host_path.as_ref().ok_or_else(|| {
            RuntimeError::InvalidInput("project host skill source path is missing".to_string())
        })?;
        copy_dir_all(host_source, &target)?;
        normalize_copied_dir(&target, &project_source.cache_name)?;
    }
    Ok(())
}

fn copy_dir_all(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest = target.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dest)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

pub(crate) fn apply_project_source_paths(
    cache_root: &Path,
    project_id: ProjectId,
    response: &mut SkillsListResponse,
) {
    apply_source_paths(&cache_dir(cache_root, project_id), response);
}

pub(crate) fn apply_source_paths(cache_dir: &Path, response: &mut SkillsListResponse) {
    for skill in &mut response.skills {
        if skill.scope != SkillScope::Project {
            continue;
        }
        if let Some(source_path) = source_path(cache_dir, &skill.path) {
            skill.source_path = Some(source_path);
        }
    }
    for error in &mut response.errors {
        if let Some(source_path) = source_path(cache_dir, &error.path) {
            error.path = source_path;
        }
    }
    response.roots = PROJECT_SKILL_CANDIDATE_DIRS
        .iter()
        .filter_map(|(relative, cache_name)| {
            let root = cache_dir.join(cache_name);
            root.exists()
                .then(|| PathBuf::from(PROJECT_WORKSPACE_PATH).join(relative))
        })
        .collect();
}

pub(crate) fn normalize_copied_dir(target: &Path, cache_name: &str) -> Result<()> {
    let nested = target.join(cache_name);
    if nested.is_dir() {
        let temp = target.with_extension("tmp");
        if temp.exists() {
            fs::remove_dir_all(&temp)?;
        }
        fs::rename(&nested, &temp)?;
        fs::remove_dir_all(target)?;
        fs::rename(temp, target)?;
    }
    Ok(())
}

fn source_path(cache_dir: &Path, path: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(cache_dir).ok()?;
    let mut components = relative.components();
    let cache_name = match components.next()? {
        std::path::Component::Normal(name) => name.to_string_lossy(),
        _ => return None,
    };
    let source_relative = PROJECT_SKILL_CANDIDATE_DIRS
        .iter()
        .find(|(_, name)| *name == cache_name.as_ref())
        .map(|(relative, _)| *relative)?;
    let mut source_path = PathBuf::from(PROJECT_WORKSPACE_PATH).join(source_relative);
    for component in components {
        match component {
            std::path::Component::Normal(part) => source_path.push(part),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(source_path)
}
