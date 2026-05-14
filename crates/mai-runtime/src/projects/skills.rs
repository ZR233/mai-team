use std::fs;
use std::path::{Path, PathBuf};

use mai_protocol::{ProjectId, SkillScope, SkillsListResponse};

use crate::Result;
use crate::projects::mcp::PROJECT_WORKSPACE_PATH;

pub(crate) const PROJECT_SKILLS_CACHE_DIR: &str = "project-skills";

const PROJECT_SKILL_CANDIDATE_DIRS: [(&str, &str); 3] = [
    (".claude/skills", "claude"),
    (".agents/skills", "agents"),
    ("skills", "skills"),
];

#[derive(Debug, Clone, Copy)]
pub(crate) enum ProjectSkillRefreshSource {
    ProjectSidecar,
    ReviewWorkspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectSkillSourceDir {
    pub(crate) cache_name: String,
    pub(crate) container_path: String,
}

impl ProjectSkillSourceDir {
    pub(crate) fn from_line(line: &str) -> Option<Self> {
        let mut parts = line.splitn(3, '\t');
        let relative = parts.next()?.trim();
        let cache_name = parts.next()?.trim();
        let container_path = parts.next()?.trim();
        if relative.is_empty() || cache_name.is_empty() || container_path.is_empty() {
            return None;
        }
        Some(Self {
            cache_name: cache_name.to_string(),
            container_path: container_path.to_string(),
        })
    }
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

pub(crate) fn source_detection_shell_checks() -> String {
    PROJECT_SKILL_CANDIDATE_DIRS
        .iter()
        .map(|(relative, cache_name)| {
            let container_path = format!("{PROJECT_WORKSPACE_PATH}/{relative}");
            format!(
                "if [ -d {path} ]; then printf '%s\\t%s\\t%s\\n' {relative} {cache_name} {path}; fi",
                path = shell_quote(&container_path),
                relative = shell_quote(relative),
                cache_name = shell_quote(cache_name),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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

fn shell_quote(value: &str) -> String {
    shell_words::quote(value).into_owned()
}
