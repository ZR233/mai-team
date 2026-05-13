use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use mai_protocol::SkillMetadata;

use crate::paths::normalized_skill_path;

#[derive(Debug, Clone, Default)]
pub struct ToolMentions {
    pub(crate) names: BTreeSet<String>,
    pub(crate) paths: BTreeSet<PathBuf>,
    pub(crate) explicit_names: BTreeSet<String>,
    blocked_plain_names: BTreeSet<String>,
}

pub fn extract_skill_mentions(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut seen = BTreeSet::<String>::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'['
            && let Some((name, _path, end)) = parse_linked_skill_mention(text, bytes, index)
        {
            if !is_common_env_var(name) && seen.insert(name.to_string()) {
                out.push(name.to_string());
            }
            index = end;
            continue;
        }
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < bytes.len() && is_mention_name_char(bytes[end]) {
            end += 1;
        }
        if end == start {
            index += 1;
            continue;
        }
        let name = &text[start..end];
        if !is_common_env_var(name) && seen.insert(name.to_string()) {
            out.push(name.to_string());
        }
        index = end;
    }
    out
}

pub fn extract_tool_mentions(text: &str) -> ToolMentions {
    let bytes = text.as_bytes();
    let mut out = ToolMentions::default();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'['
            && let Some((name, path, end)) = parse_linked_skill_mention(text, bytes, index)
        {
            if !is_common_env_var(name) {
                out.names.insert(name.to_string());
                out.paths.insert(normalized_skill_path(Path::new(path)));
                out.blocked_plain_names.insert(name.to_string());
            }
            index = end;
            continue;
        }
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < bytes.len() && is_mention_name_char(bytes[end]) {
            end += 1;
        }
        if end == start {
            index += 1;
            continue;
        }
        let name = &text[start..end];
        if !is_common_env_var(name) {
            out.names.insert(name.to_string());
            out.explicit_names.insert(name.to_string());
        }
        index = end;
    }
    out
}

impl ToolMentions {
    pub(crate) fn blocked_plain_names(&self) -> impl Iterator<Item = String> + '_ {
        self.blocked_plain_names.iter().cloned()
    }
}

pub(crate) fn normalize_mention(value: &str) -> &str {
    value.trim().trim_start_matches('$')
}

pub(crate) fn extract_plain_skill_names(text: &str, skills: &[&SkillMetadata]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for skill in skills {
        if contains_plain_skill_name(text, &skill.name) {
            names.insert(skill.name.clone());
        }
    }
    names
}

fn contains_plain_skill_name(text: &str, name: &str) -> bool {
    if name.is_empty() || is_common_env_var(name) {
        return false;
    }
    let bytes = text.as_bytes();
    let name_len = name.len();
    let mut index = 0;
    while index < text.len() {
        let Some(relative_start) = text[index..].find(name) else {
            return false;
        };
        let start = index + relative_start;
        let end = start + name_len;
        let before_ok = start == 0 || !is_mention_name_char(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_mention_name_char(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        index = end;
    }
    false
}

fn parse_linked_skill_mention<'a>(
    text: &'a str,
    bytes: &[u8],
    start: usize,
) -> Option<(&'a str, &'a str, usize)> {
    let sigil_index = start + 1;
    if bytes.get(sigil_index) != Some(&b'$') {
        return None;
    }

    let name_start = sigil_index + 1;
    let first_name_byte = bytes.get(name_start)?;
    if !is_mention_name_char(*first_name_byte) {
        return None;
    }

    let mut name_end = name_start + 1;
    while let Some(next_byte) = bytes.get(name_end)
        && is_mention_name_char(*next_byte)
    {
        name_end += 1;
    }

    if bytes.get(name_end) != Some(&b']') {
        return None;
    }

    let mut path_start = name_end + 1;
    while let Some(next_byte) = bytes.get(path_start)
        && next_byte.is_ascii_whitespace()
    {
        path_start += 1;
    }
    if bytes.get(path_start) != Some(&b'(') {
        return None;
    }

    let mut path_end = path_start + 1;
    while let Some(next_byte) = bytes.get(path_end)
        && *next_byte != b')'
    {
        path_end += 1;
    }
    if bytes.get(path_end) != Some(&b')') {
        return None;
    }

    let path = text[path_start + 1..path_end].trim();
    if path.is_empty() {
        return None;
    }

    let name = &text[name_start..name_end];
    Some((name, path, path_end + 1))
}

fn is_mention_name_char(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b':')
}

fn is_common_env_var(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "PATH"
            | "HOME"
            | "USER"
            | "SHELL"
            | "PWD"
            | "TMPDIR"
            | "TEMP"
            | "TMP"
            | "LANG"
            | "TERM"
            | "XDG_CONFIG_HOME"
    )
}
