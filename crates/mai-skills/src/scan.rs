use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use mai_protocol::{SkillErrorInfo, SkillMetadata, SkillScope};
use walkdir::{DirEntry, WalkDir};

use crate::constants::{MAX_SCAN_DEPTH, SKILL_FILE};
use crate::ordering::skill_sort;
use crate::parser::parse_skill_file;
use crate::paths::canonicalize_or_clone;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillRoot {
    pub(crate) path: PathBuf,
    pub(crate) scope: SkillScope,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SkillLoadOutcome {
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) skills: Vec<SkillMetadata>,
    pub(crate) errors: Vec<SkillErrorInfo>,
}

pub(crate) fn default_roots(repo_root: &Path) -> Vec<SkillRoot> {
    default_roots_with_system(repo_root, None)
}

pub(crate) fn default_roots_with_system(
    repo_root: &Path,
    system_root: Option<&Path>,
) -> Vec<SkillRoot> {
    let mut roots = vec![SkillRoot {
        path: repo_root.join(".agents").join("skills"),
        scope: SkillScope::Repo,
    }];
    if let Some(home) = dirs::home_dir() {
        roots.push(SkillRoot {
            path: home.join(".agents").join("skills"),
            scope: SkillScope::User,
        });
    }
    if let Some(system_root) = system_root {
        roots.push(SkillRoot {
            path: system_root.to_path_buf(),
            scope: SkillScope::System,
        });
    }
    roots
}

pub(crate) fn roots_from_pairs(roots: Vec<(PathBuf, SkillScope)>) -> Vec<SkillRoot> {
    roots
        .into_iter()
        .map(|(path, scope)| SkillRoot { path, scope })
        .collect()
}

pub(crate) fn scan_roots(roots: &[SkillRoot]) -> SkillLoadOutcome {
    let mut outcome = SkillLoadOutcome::default();
    let mut seen_paths = BTreeSet::<PathBuf>::new();

    for root in roots {
        let canonical_root = canonicalize_or_clone(&root.path);
        if !outcome.roots.contains(&canonical_root) {
            outcome.roots.push(canonical_root);
        }
        if !root.path.exists() {
            continue;
        }

        for entry in WalkDir::new(&root.path)
            .follow_links(true)
            .max_depth(MAX_SCAN_DEPTH)
            .into_iter()
            .filter_entry(include_entry)
        {
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_file() || entry.file_name() != SKILL_FILE {
                continue;
            }
            let path = entry.path().to_path_buf();
            let canonical = canonicalize_or_clone(&path);
            if !seen_paths.insert(canonical.clone()) {
                continue;
            }
            match parse_skill_file(&path, root.scope) {
                Ok(mut skill) => {
                    skill.path = canonical;
                    outcome.skills.push(skill);
                }
                Err(err) => outcome.errors.push(SkillErrorInfo {
                    path: canonical,
                    message: err.to_string(),
                }),
            }
        }
    }

    outcome.skills.sort_by(skill_sort);
    outcome
}

fn include_entry(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    entry.depth() == 0 || !name.starts_with('.')
}
