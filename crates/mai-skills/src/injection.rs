use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use mai_protocol::SkillMetadata;

use crate::mentions::{extract_plain_skill_names, extract_tool_mentions, normalize_mention};
use crate::paths::{canonicalize_or_clone, looks_like_path, normalized_skill_path};
use crate::scan::SkillLoadOutcome;

#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub metadata: SkillMetadata,
    pub contents: String,
}

#[derive(Debug, Clone, Default)]
pub struct SkillInjections {
    pub items: Vec<LoadedSkill>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SkillInput<'a> {
    pub text: Option<&'a str>,
    pub selections: Vec<SkillSelection>,
    pub reserved_names: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSelection {
    pub name: Option<String>,
    pub path: Option<PathBuf>,
}

impl SkillSelection {
    pub fn from_mention(value: impl Into<String>) -> Self {
        let value = value.into();
        if looks_like_path(normalize_mention(&value)) {
            Self {
                name: None,
                path: Some(PathBuf::from(normalize_mention(&value))),
            }
        } else {
            Self {
                name: Some(normalize_mention(&value).to_string()),
                path: None,
            }
        }
    }
}

pub(crate) fn build_injections_from_outcome(
    outcome: &SkillLoadOutcome,
    input: &SkillInput<'_>,
) -> SkillInjections {
    let mut result = SkillInjections::default();
    let mut seen_paths = BTreeSet::<PathBuf>::new();
    let mut blocked_plain_names = BTreeSet::<String>::new();
    let mut selection_names = BTreeSet::<String>::new();
    let mut explicit_names = BTreeSet::<String>::new();
    let mut plain_text_names = BTreeSet::<String>::new();
    let mut paths = BTreeSet::<PathBuf>::new();
    let enabled = outcome
        .skills
        .iter()
        .filter(|skill| skill.enabled)
        .collect::<Vec<_>>();
    let name_counts = skill_name_counts(&enabled);

    for selection in &input.selections {
        if let Some(path) = &selection.path {
            paths.insert(normalized_skill_path(path));
        }
        if let Some(name) = &selection.name {
            let name = normalize_mention(name).trim();
            if !name.is_empty() {
                selection_names.insert(name.to_string());
            }
        }
    }

    if let Some(text) = input.text {
        let mentions = extract_tool_mentions(text);
        blocked_plain_names.extend(mentions.blocked_plain_names());
        paths.extend(mentions.paths);
        explicit_names.extend(mentions.explicit_names);
        plain_text_names.extend(extract_plain_skill_names(text, &enabled));
    }

    for path in &paths {
        let path = canonicalize_or_clone(path);
        if let Some(skill) = enabled.iter().find(|skill| skill.path == path) {
            blocked_plain_names.insert(skill.name.clone());
            load_skill_contents(skill, &mut seen_paths, &mut result);
        }
    }

    for name in selection_names {
        load_unique_skill(
            &name,
            &SkillLookupContext {
                enabled: &enabled,
                blocked: &blocked_plain_names,
                reserved: &BTreeSet::new(),
                name_counts: &name_counts,
            },
            false,
            &mut seen_paths,
            &mut result,
        );
    }

    for name in explicit_names {
        load_unique_skill(
            &name,
            &SkillLookupContext {
                enabled: &enabled,
                blocked: &blocked_plain_names,
                reserved: &input.reserved_names,
                name_counts: &name_counts,
            },
            false,
            &mut seen_paths,
            &mut result,
        );
    }

    for name in plain_text_names {
        load_unique_skill(
            &name,
            &SkillLookupContext {
                enabled: &enabled,
                blocked: &blocked_plain_names,
                reserved: &input.reserved_names,
                name_counts: &name_counts,
            },
            true,
            &mut seen_paths,
            &mut result,
        );
    }

    result
}

struct SkillLookupContext<'a> {
    enabled: &'a [&'a SkillMetadata],
    blocked: &'a BTreeSet<String>,
    reserved: &'a BTreeSet<String>,
    name_counts: &'a BTreeMap<String, usize>,
}

fn load_unique_skill(
    name: &str,
    ctx: &SkillLookupContext<'_>,
    require_implicit: bool,
    seen_paths: &mut BTreeSet<PathBuf>,
    result: &mut SkillInjections,
) {
    if ctx.blocked.contains(name)
        || ctx.reserved.contains(name)
        || ctx.name_counts.get(name).copied().unwrap_or_default() != 1
    {
        return;
    }
    if let Some(skill) = ctx.enabled.iter().find(|skill| skill.name == name)
        && (!require_implicit || skill_allows_implicit(skill))
    {
        load_skill_contents(skill, seen_paths, result);
    }
}

fn load_skill_contents(
    skill: &SkillMetadata,
    seen_paths: &mut BTreeSet<PathBuf>,
    result: &mut SkillInjections,
) {
    if !seen_paths.insert(skill.path.clone()) {
        return;
    }
    match fs::read_to_string(&skill.path) {
        Ok(contents) => result.items.push(LoadedSkill {
            metadata: skill.clone(),
            contents,
        }),
        Err(err) => result.warnings.push(format!(
            "Failed to load skill {name} at {path}: {err}",
            name = skill.name,
            path = skill.path.display()
        )),
    }
}

fn skill_name_counts(skills: &[&SkillMetadata]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for skill in skills {
        *counts.entry(skill.name.clone()).or_default() += 1;
    }
    counts
}

fn skill_allows_implicit(skill: &SkillMetadata) -> bool {
    skill
        .policy
        .as_ref()
        .and_then(|policy| policy.allow_implicit_invocation)
        .unwrap_or(true)
}
