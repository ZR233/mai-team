use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex as StdMutex;

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/system-skills"]
struct EmbeddedSystemSkills;

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/system-agents"]
struct EmbeddedSystemAgents;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddedResourceRoot {
    Skills,
    Agents,
}

impl EmbeddedResourceRoot {
    fn out_dir_name(self) -> &'static str {
        match self {
            Self::Skills => "system-skills",
            Self::Agents => "system-agents",
        }
    }
}

static EMBEDDED_RESOURCE_RELEASE_LOCK: StdMutex<()> = StdMutex::new(());

pub(crate) fn release_embedded_system_skills(target_dir: &Path) -> io::Result<()> {
    release_embedded_resources::<EmbeddedSystemSkills>(target_dir, EmbeddedResourceRoot::Skills)
}

pub(crate) fn release_embedded_system_agents(target_dir: &Path) -> io::Result<()> {
    release_embedded_resources::<EmbeddedSystemAgents>(target_dir, EmbeddedResourceRoot::Agents)
}

fn release_embedded_resources<E>(
    target_dir: &Path,
    resource_root: EmbeddedResourceRoot,
) -> io::Result<()>
where
    E: RustEmbed,
{
    let _guard = EMBEDDED_RESOURCE_RELEASE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !safe_system_resource_target(target_dir) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsafe system resource target: {}", target_dir.display()),
        ));
    }
    if target_dir.exists() {
        std::fs::remove_dir_all(target_dir)?;
    }
    std::fs::create_dir_all(target_dir)?;
    for path in E::iter() {
        let path = path.as_ref();
        let Some(relative) = embedded_system_resource_relative_path(path, resource_root) else {
            continue;
        };
        let target = target_dir.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(asset) = E::get(path) {
            std::fs::write(target, asset.data.as_ref())?;
        }
    }
    Ok(())
}

fn safe_system_resource_target(path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return false;
    }
    !matches!(
        path.components().next_back(),
        None | Some(Component::RootDir | Component::Prefix(_))
    )
}

fn embedded_system_resource_relative_path(
    path: &str,
    resource_root: EmbeddedResourceRoot,
) -> Option<PathBuf> {
    let path = Path::new(path);
    let out_dir_name = resource_root.out_dir_name();
    let relative = if path.is_absolute() {
        path.strip_prefix(Path::new(env!("OUT_DIR")).join(out_dir_name))
            .ok()?
    } else {
        path
    };
    let relative = relative.strip_prefix(out_dir_name).unwrap_or(relative);
    safe_embedded_relative_path_from_path(relative)
}

fn safe_embedded_relative_path_from_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

#[cfg(test)]
fn embedded_system_skill_relative_path(path: &str) -> Option<PathBuf> {
    embedded_system_resource_relative_path(path, EmbeddedResourceRoot::Skills)
}

#[cfg(test)]
fn embedded_system_agent_relative_path(path: &str) -> Option<PathBuf> {
    embedded_system_resource_relative_path(path, EmbeddedResourceRoot::Agents)
}

#[cfg(test)]
fn safe_embedded_relative_path(path: &str) -> Option<PathBuf> {
    safe_embedded_relative_path_from_path(Path::new(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn embedded_system_skills_release_to_target_dir() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("system-skills");

        release_embedded_system_skills(&target).expect("release skills");

        let skill_path = target.join("reviewer-agent-review-pr").join("SKILL.md");
        let contents = fs::read_to_string(skill_path).expect("skill contents");
        assert!(contents.contains("name: reviewer-agent-review-pr"));
    }

    #[test]
    fn embedded_system_agents_release_to_target_dir() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("system-agents");

        release_embedded_system_agents(&target).expect("release agents");

        let maintainer_path = target.join("project-maintainer").join("AGENT.md");
        let reviewer_path = target.join("project-reviewer").join("AGENT.md");
        let contents = fs::read_to_string(maintainer_path).expect("agent contents");
        assert!(contents.contains("id: project-maintainer"));
        assert!(reviewer_path.exists());
    }

    #[test]
    fn embedded_system_skills_release_overwrites_target_dir() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("system-skills");
        fs::create_dir_all(&target).expect("mkdir");
        fs::write(target.join("stale.txt"), "old").expect("write stale");

        release_embedded_system_skills(&target).expect("release skills");

        assert!(!target.join("stale.txt").exists());
        let expected = target.join("reviewer-agent-review-pr").join("SKILL.md");
        assert!(
            expected.exists(),
            "expected {}, found {:?}",
            expected.display(),
            list_relative_files(&target)
        );
    }

    fn list_relative_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                collect_relative_files(root, &entry.path(), &mut files);
            }
        }
        files.sort();
        files
    }

    fn collect_relative_files(root: &Path, path: &Path, files: &mut Vec<PathBuf>) {
        if path.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    collect_relative_files(root, &entry.path(), files);
                }
            }
        } else if let Ok(relative) = path.strip_prefix(root) {
            files.push(relative.to_path_buf());
        }
    }

    #[test]
    fn safe_embedded_relative_path_rejects_parent_components() {
        assert_eq!(
            safe_embedded_relative_path("reviewer-agent-review-pr/SKILL.md"),
            Some(PathBuf::from("reviewer-agent-review-pr").join("SKILL.md"))
        );
        assert_eq!(
            embedded_system_skill_relative_path("system-skills/reviewer-agent-review-pr/SKILL.md"),
            Some(PathBuf::from("reviewer-agent-review-pr").join("SKILL.md"))
        );
        assert_eq!(safe_embedded_relative_path("../SKILL.md"), None);
        assert_eq!(safe_embedded_relative_path("/tmp/SKILL.md"), None);
        assert_eq!(
            embedded_system_skill_relative_path(
                &Path::new(env!("OUT_DIR"))
                    .join("system-skills")
                    .join("reviewer-agent-review-pr")
                    .join("SKILL.md")
                    .to_string_lossy()
            ),
            Some(PathBuf::from("reviewer-agent-review-pr").join("SKILL.md"))
        );
        assert_eq!(
            embedded_system_agent_relative_path(
                &Path::new(env!("OUT_DIR"))
                    .join("system-agents")
                    .join("project-maintainer")
                    .join("AGENT.md")
                    .to_string_lossy()
            ),
            Some(PathBuf::from("project-maintainer").join("AGENT.md"))
        );
    }

    #[test]
    fn system_skills_release_rejects_root_target() {
        assert!(!safe_system_resource_target(Path::new("")));
        assert!(!safe_system_resource_target(Path::new("/")));
        assert!(safe_system_resource_target(Path::new("/tmp/system-skills")));
    }
}
