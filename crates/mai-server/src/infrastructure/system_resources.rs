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
    release_embedded_resources::<EmbeddedSystemSkills>(target_dir, EmbeddedResourceRoot::Skills)?;
    if let Some(source_dir) = development_system_resource_source(EmbeddedResourceRoot::Skills) {
        overlay_system_resource_directory(&source_dir, target_dir)?;
    }
    Ok(())
}

pub(crate) fn release_embedded_system_agents(target_dir: &Path) -> io::Result<()> {
    release_embedded_resources::<EmbeddedSystemAgents>(target_dir, EmbeddedResourceRoot::Agents)?;
    if let Some(source_dir) = development_system_resource_source(EmbeddedResourceRoot::Agents) {
        overlay_system_resource_directory(&source_dir, target_dir)?;
    }
    Ok(())
}

fn development_system_resource_source(resource_root: EmbeddedResourceRoot) -> Option<PathBuf> {
    let source_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(resource_root.out_dir_name());
    if cfg!(debug_assertions) && source_dir.is_dir() {
        Some(source_dir)
    } else {
        None
    }
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
    reset_system_resource_target(target_dir)?;
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

fn reset_system_resource_target(target_dir: &Path) -> io::Result<()> {
    if !safe_system_resource_target(target_dir) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsafe system resource target: {}", target_dir.display()),
        ));
    }
    if target_dir.exists() {
        std::fs::remove_dir_all(target_dir)?;
    }
    std::fs::create_dir_all(target_dir)
}

fn overlay_system_resource_directory(source_dir: &Path, target_dir: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(source_dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        if matches!(file_name.to_str(), Some(".DS_Store")) {
            continue;
        }
        let target = target_dir.join(&file_name);
        if target.is_dir() {
            std::fs::remove_dir_all(&target)?;
        } else if target.exists() {
            std::fs::remove_file(&target)?;
        }
        let source = entry.path();
        if source.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_system_resource_directory(&source, &target)?;
        } else {
            std::fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

fn copy_system_resource_directory(source_dir: &Path, target_dir: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(source_dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        if matches!(file_name.to_str(), Some(".DS_Store")) {
            continue;
        }
        let source = entry.path();
        let target = target_dir.join(file_name);
        if source.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_system_resource_directory(&source, &target)?;
        } else {
            std::fs::copy(&source, &target)?;
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
        assert!(contents.contains("`write_session_note` once with `expectedRevision: 0`"));
        assert!(contents.contains("Initialize the note with immutable metadata only"));
        assert!(contents.contains("Do not add progress checkboxes"));
        assert!(contents.contains("immediately append one complete Markdown block"));
        assert!(contents.contains("`apply_session_note_patch`"));
        for field in [
            "- Status:",
            "- Severity:",
            "- File:",
            "- Lines:",
            "- Inline disposition:",
            "- Problem:",
            "- Impact and evidence:",
            "- Suggested fix:",
            "- Proposed review text:",
        ] {
            assert!(contents.contains(field), "missing finding field {field}");
        }
        assert!(contents.contains("must never replace or delete an existing line"));
        assert!(contents.contains("using the real current head line"));
        assert!(contents.contains("Never update a progress checklist"));
        assert!(contents.contains("never patch the earlier block in place"));
        assert!(contents.contains("read the entire findings ledger"));
        assert!(contents.contains("`startLine: 1`"));
        assert!(contents.contains("`maxLines: 500`"));
        assert!(contents.contains("latest known revision as `expectedRevision`"));
        assert!(contents.contains("each non-empty `nextStartLine`"));
        assert!(contents.contains("restart paginated reading from line 1"));
        assert!(contents.contains("do not use its optional cursor"));
        assert!(contents.contains("one logical final review request"));
        assert!(!contents.contains("POSIX `sh`"));
        assert!(!contents.contains("`bash -lc`"));
        assert!(!contents.contains("/tmp/mai-review-findings.md"));
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
        let reviewer_contents = fs::read_to_string(reviewer_path).expect("reviewer contents");
        assert!(reviewer_contents.contains("`write_session_note`"));
        assert!(reviewer_contents.contains("`apply_session_note_patch`"));
        assert!(reviewer_contents.contains("`search_session_note`"));
        assert!(reviewer_contents.contains("`read_session_note`"));
        assert!(reviewer_contents.contains("one logical final pull request review"));
        assert!(!reviewer_contents.contains("POSIX `sh`"));
        assert!(!reviewer_contents.contains("`bash -lc`"));
        assert!(!reviewer_contents.contains("/tmp/mai-review-findings.md"));
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

    #[test]
    fn debug_system_skills_release_overlays_source_tree() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("system-skills");

        release_embedded_system_skills(&target).expect("release skills");

        let skill_path = target.join("reviewer-agent-review-pr").join("SKILL.md");
        let contents = fs::read_to_string(skill_path).expect("skill contents");
        assert!(contents.contains("system selector is responsible for choosing the PR"));
        assert!(!contents.contains("select-pr --prs"));
    }

    #[test]
    fn debug_system_agents_release_overlays_source_tree() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("system-agents");

        release_embedded_system_agents(&target).expect("release agents");

        let reviewer_path = target.join("project-reviewer").join("AGENT.md");
        let contents = fs::read_to_string(reviewer_path).expect("agent contents");
        assert!(contents.contains("review the one pull request selected by Mai"));
        assert!(!contents.contains("one eligible pull request"));
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
