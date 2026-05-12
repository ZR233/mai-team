use mai_protocol::{
    SkillConfigEntry, SkillDependencies, SkillErrorInfo, SkillInterface, SkillMetadata,
    SkillPolicy, SkillScope, SkillToolDependency, SkillsConfigRequest, SkillsListResponse,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;
use walkdir::{DirEntry, WalkDir};

const SKILL_FILE: &str = "SKILL.md";
const METADATA_DIR: &str = "agents";
const METADATA_FILE: &str = "openai.yaml";
const MAX_SCAN_DEPTH: usize = 6;
const MAX_NAME_LEN: usize = 64;
const MAX_DESCRIPTION_LEN: usize = 8192;
const SKILL_PATH_PREFIX: &str = "skill://";

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("invalid skill config: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, SkillError>;

#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub metadata: SkillMetadata,
    pub contents: String,
}

#[derive(Debug, Clone, Default)]
pub struct SkillInjections {
    pub items: Vec<LoadedSkill>,
    pub suggestions: Vec<SkillSuggestion>,
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

#[derive(Debug, Clone, Default)]
pub struct ToolMentions {
    names: BTreeSet<String>,
    paths: BTreeSet<PathBuf>,
    explicit_names: BTreeSet<String>,
    blocked_plain_names: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSuggestion {
    pub name: String,
    pub display_name: Option<String>,
    pub description: String,
    pub path: PathBuf,
    pub scope: SkillScope,
    pub reason: String,
    pub score: i32,
}

#[derive(Debug, Clone)]
pub struct SkillsManager {
    roots: Vec<SkillRoot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillRoot {
    path: PathBuf,
    scope: SkillScope,
}

#[derive(Debug, Clone, Default)]
struct SkillLoadOutcome {
    roots: Vec<PathBuf>,
    skills: Vec<SkillMetadata>,
    errors: Vec<SkillErrorInfo>,
}

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    metadata: SkillFrontmatterMetadata,
}

#[derive(Debug, Default, Deserialize)]
struct SkillFrontmatterMetadata {
    #[serde(default, rename = "short-description")]
    short_description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SkillMetadataFile {
    #[serde(default)]
    interface: Option<OpenAiInterface>,
    #[serde(default)]
    dependencies: Option<OpenAiDependencies>,
    #[serde(default)]
    policy: Option<OpenAiPolicy>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiInterface {
    display_name: Option<String>,
    short_description: Option<String>,
    icon_small: Option<PathBuf>,
    icon_large: Option<PathBuf>,
    brand_color: Option<String>,
    default_prompt: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiDependencies {
    #[serde(default)]
    tools: Vec<OpenAiToolDependency>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiToolDependency {
    #[serde(rename = "type")]
    kind: Option<String>,
    value: Option<String>,
    description: Option<String>,
    transport: Option<String>,
    command: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiPolicy {
    #[serde(default)]
    allow_implicit_invocation: Option<bool>,
}

#[derive(Debug)]
enum SkillParseError {
    Read(std::io::Error),
    MissingFrontmatter,
    InvalidYaml(serde_yaml::Error),
    MissingField(&'static str),
    InvalidField { field: &'static str, reason: String },
}

impl fmt::Display for SkillParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkillParseError::Read(err) => write!(f, "failed to read file: {err}"),
            SkillParseError::MissingFrontmatter => {
                write!(f, "missing YAML frontmatter delimited by ---")
            }
            SkillParseError::InvalidYaml(err) => write!(f, "invalid YAML: {err}"),
            SkillParseError::MissingField(field) => write!(f, "missing field `{field}`"),
            SkillParseError::InvalidField { field, reason } => {
                write!(f, "invalid {field}: {reason}")
            }
        }
    }
}

impl Error for SkillParseError {}

impl SkillsManager {
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            roots: default_roots(repo_root.as_ref()),
        }
    }

    pub fn new_with_system_root(
        repo_root: impl AsRef<Path>,
        system_root: Option<impl AsRef<Path>>,
    ) -> Self {
        Self {
            roots: default_roots_with_system(
                repo_root.as_ref(),
                system_root.as_ref().map(|path| path.as_ref()),
            ),
        }
    }

    pub fn new_with_system_root_and_extra_roots(
        repo_root: impl AsRef<Path>,
        system_root: Option<impl AsRef<Path>>,
        extra_roots: Vec<(PathBuf, SkillScope)>,
    ) -> Self {
        let mut roots = default_roots_with_system(
            repo_root.as_ref(),
            system_root.as_ref().map(|path| path.as_ref()),
        );
        roots.extend(
            extra_roots
                .into_iter()
                .map(|(path, scope)| SkillRoot { path, scope }),
        );
        Self { roots }
    }

    pub fn with_roots(roots: Vec<(PathBuf, SkillScope)>) -> Self {
        Self {
            roots: roots
                .into_iter()
                .map(|(path, scope)| SkillRoot { path, scope })
                .collect(),
        }
    }

    pub fn root_paths(&self) -> Vec<PathBuf> {
        self.roots.iter().map(|root| root.path.clone()).collect()
    }

    pub fn clone_with_extra_roots(&self, extra_roots: Vec<(PathBuf, SkillScope)>) -> Self {
        let mut roots = self.roots.clone();
        roots.extend(
            extra_roots
                .into_iter()
                .map(|(path, scope)| SkillRoot { path, scope }),
        );
        Self { roots }
    }

    pub fn list(&self, config: &SkillsConfigRequest) -> Result<SkillsListResponse> {
        let mut outcome = self.load_outcome();
        apply_config(&mut outcome.skills, config)?;
        Ok(SkillsListResponse {
            roots: outcome.roots,
            skills: outcome.skills,
            errors: outcome.errors,
        })
    }

    pub fn render_available(&self, config: &SkillsConfigRequest) -> Result<String> {
        Ok(render_available_response(self.list(config)?))
    }

    pub fn build_injections(
        &self,
        explicit_mentions: &[String],
        config: &SkillsConfigRequest,
    ) -> Result<SkillInjections> {
        let mut outcome = self.load_outcome();
        apply_config(&mut outcome.skills, config)?;
        Ok(build_injections_from_outcome(
            &outcome,
            &SkillInput {
                selections: explicit_mentions
                    .iter()
                    .map(|mention| SkillSelection::from_mention(mention.clone()))
                    .collect(),
                ..Default::default()
            },
        ))
    }

    pub fn build_injections_for_message(
        &self,
        message: &str,
        explicit_mentions: &[String],
        config: &SkillsConfigRequest,
    ) -> Result<SkillInjections> {
        let mut outcome = self.load_outcome();
        apply_config(&mut outcome.skills, config)?;
        Ok(build_injections_from_outcome(
            &outcome,
            &SkillInput {
                text: Some(message),
                selections: explicit_mentions
                    .iter()
                    .map(|mention| SkillSelection::from_mention(mention.clone()))
                    .collect(),
                ..Default::default()
            },
        ))
    }

    pub fn build_injections_for_input(
        &self,
        input: SkillInput<'_>,
        config: &SkillsConfigRequest,
    ) -> Result<SkillInjections> {
        let mut outcome = self.load_outcome();
        apply_config(&mut outcome.skills, config)?;
        Ok(build_injections_from_outcome(&outcome, &input))
    }

    fn load_outcome(&self) -> SkillLoadOutcome {
        let mut outcome = SkillLoadOutcome::default();
        let mut seen_paths = BTreeSet::<PathBuf>::new();

        for root in &self.roots {
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
}

pub fn render_available_response(response: SkillsListResponse) -> String {
    let mut skills = response
        .skills
        .into_iter()
        .filter(|skill| {
            skill.enabled
                && skill
                    .policy
                    .as_ref()
                    .and_then(|policy| policy.allow_implicit_invocation)
                    .unwrap_or(true)
        })
        .collect::<Vec<_>>();
    if skills.is_empty() {
        return "No skills are currently available.".to_string();
    }

    skills.sort_by(skill_sort);

    let mut lines = Vec::with_capacity(skills.len() + 7);
    lines.push("A skill is a set of local instructions to follow that is stored in a `SKILL.md` file. Below is the list of skills that can be used. Each entry includes a name, description, and file path so you can open the source for full instructions when using a specific skill.".to_string());
    lines.push("### Available Skills".to_string());
    for skill in skills {
        lines.push(format!(
            "- ${}: {} (path: {})",
            skill.name,
            display_description(&skill),
            skill_display_path(&skill).display()
        ));
    }
    lines.push("### How to use skills".to_string());
    lines.push("- Discovery: The list above is the skills available in this session (name + description + file path). Skill bodies live on disk at the listed paths.".to_string());
    lines.push("- Trigger rules: If the user names a skill (with `$SkillName` or plain text), selects it from the UI, or the task clearly matches a skill's description shown above, you must use that skill for that turn. Multiple mentions mean use them all. Do not carry skills across turns unless re-mentioned.".to_string());
    lines.push("- Missing/blocked: If a named skill is not in the list or the path cannot be read, say so briefly and continue with the best fallback.".to_string());
    lines.push("- How to use a skill: after deciding to use a skill, open its `SKILL.md`. Read only enough to follow the workflow. Resolve relative references from the directory containing that `SKILL.md`; if it points to `references/`, `scripts/`, or `assets/`, load only the specific files needed.".to_string());
    lines.push("- Coordination and context hygiene: if multiple skills apply, choose the minimal set that covers the request and state the order you will use them. Keep context small and avoid deep reference-chasing unless blocked.".to_string());
    lines.join("\n")
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
    fn blocked_plain_names(&self) -> impl Iterator<Item = String> + '_ {
        self.blocked_plain_names.iter().cloned()
    }
}

pub fn normalize_config(config: &SkillsConfigRequest) -> Result<SkillsConfigRequest> {
    let mut out = SkillsConfigRequest::default();
    for entry in &config.config {
        match (entry.path.as_ref(), entry.name.as_deref()) {
            (Some(path), None) => out.config.push(SkillConfigEntry {
                path: Some(canonicalize_or_clone(path)),
                name: None,
                enabled: entry.enabled,
            }),
            (None, Some(name)) => {
                let name = name.trim();
                if name.is_empty() {
                    return Err(SkillError::InvalidConfig(
                        "skill config name cannot be empty".to_string(),
                    ));
                }
                out.config.push(SkillConfigEntry {
                    name: Some(name.to_string()),
                    path: None,
                    enabled: entry.enabled,
                });
            }
            (Some(_), Some(_)) => {
                return Err(SkillError::InvalidConfig(
                    "skill config entries must use either path or name, not both".to_string(),
                ));
            }
            (None, None) => {
                return Err(SkillError::InvalidConfig(
                    "skill config entries must include path or name".to_string(),
                ));
            }
        }
    }
    Ok(out)
}

fn default_roots(repo_root: &Path) -> Vec<SkillRoot> {
    default_roots_with_system(repo_root, None)
}

fn default_roots_with_system(repo_root: &Path, system_root: Option<&Path>) -> Vec<SkillRoot> {
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

fn include_entry(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    entry.depth() == 0 || !name.starts_with('.')
}

fn parse_skill_file(
    path: &Path,
    scope: SkillScope,
) -> std::result::Result<SkillMetadata, SkillParseError> {
    let contents = fs::read_to_string(path).map_err(SkillParseError::Read)?;
    let frontmatter = extract_frontmatter(&contents).ok_or(SkillParseError::MissingFrontmatter)?;
    let parsed: SkillFrontmatter =
        serde_yaml::from_str(frontmatter).map_err(SkillParseError::InvalidYaml)?;
    let name = required_single_line(parsed.name, "name")?;
    let description = required_single_line(parsed.description, "description")?;
    validate_len(&name, MAX_NAME_LEN, "name")?;
    validate_len(&description, MAX_DESCRIPTION_LEN, "description")?;
    let short_description = parsed
        .metadata
        .short_description
        .and_then(|value| optional_single_line(value, "metadata.short-description"));
    let metadata = load_openai_metadata(path);

    Ok(SkillMetadata {
        name,
        description,
        short_description,
        path: path.to_path_buf(),
        source_path: None,
        scope,
        enabled: true,
        interface: metadata.interface,
        dependencies: metadata.dependencies,
        policy: metadata.policy,
    })
}

#[derive(Default)]
struct LoadedOpenAiMetadata {
    interface: Option<SkillInterface>,
    dependencies: Option<SkillDependencies>,
    policy: Option<SkillPolicy>,
}

fn load_openai_metadata(skill_path: &Path) -> LoadedOpenAiMetadata {
    let Some(skill_dir) = skill_path.parent() else {
        return LoadedOpenAiMetadata::default();
    };
    let metadata_path = skill_dir.join(METADATA_DIR).join(METADATA_FILE);
    let Ok(contents) = fs::read_to_string(&metadata_path) else {
        return LoadedOpenAiMetadata::default();
    };
    let Ok(parsed) = serde_yaml::from_str::<SkillMetadataFile>(&contents) else {
        return LoadedOpenAiMetadata::default();
    };
    LoadedOpenAiMetadata {
        interface: resolve_interface(parsed.interface, skill_dir),
        dependencies: resolve_dependencies(parsed.dependencies),
        policy: parsed.policy.map(|policy| SkillPolicy {
            allow_implicit_invocation: policy.allow_implicit_invocation,
        }),
    }
}

fn resolve_interface(
    interface: Option<OpenAiInterface>,
    skill_dir: &Path,
) -> Option<SkillInterface> {
    let interface = interface?;
    let resolved = SkillInterface {
        display_name: interface
            .display_name
            .and_then(|value| optional_single_line(value, "interface.display_name")),
        short_description: interface
            .short_description
            .and_then(|value| optional_single_line(value, "interface.short_description")),
        icon_small: resolve_asset_path(skill_dir, interface.icon_small),
        icon_large: resolve_asset_path(skill_dir, interface.icon_large),
        brand_color: interface
            .brand_color
            .and_then(|value| optional_single_line(value, "interface.brand_color")),
        default_prompt: interface
            .default_prompt
            .and_then(|value| optional_single_line(value, "interface.default_prompt")),
    };
    if resolved.display_name.is_some()
        || resolved.short_description.is_some()
        || resolved.icon_small.is_some()
        || resolved.icon_large.is_some()
        || resolved.brand_color.is_some()
        || resolved.default_prompt.is_some()
    {
        Some(resolved)
    } else {
        None
    }
}

fn resolve_dependencies(dependencies: Option<OpenAiDependencies>) -> Option<SkillDependencies> {
    let dependencies = dependencies?;
    let tools = dependencies
        .tools
        .into_iter()
        .filter_map(resolve_tool_dependency)
        .collect::<Vec<_>>();
    (!tools.is_empty()).then_some(SkillDependencies { tools })
}

fn resolve_tool_dependency(tool: OpenAiToolDependency) -> Option<SkillToolDependency> {
    let kind = tool
        .kind
        .and_then(|value| optional_single_line(value, "dependencies.tools.type"))?;
    let value = tool
        .value
        .and_then(|value| optional_single_line(value, "dependencies.tools.value"))?;
    Some(SkillToolDependency {
        kind,
        value,
        description: tool
            .description
            .and_then(|value| optional_single_line(value, "dependencies.tools.description")),
        transport: tool
            .transport
            .and_then(|value| optional_single_line(value, "dependencies.tools.transport")),
        command: tool
            .command
            .and_then(|value| optional_single_line(value, "dependencies.tools.command")),
        url: tool
            .url
            .and_then(|value| optional_single_line(value, "dependencies.tools.url")),
    })
}

fn resolve_asset_path(skill_dir: &Path, path: Option<PathBuf>) -> Option<PathBuf> {
    let path = path?;
    if path.is_absolute() || path.as_os_str().is_empty() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            _ => return None,
        }
    }
    let mut components = normalized.components();
    match components.next() {
        Some(Component::Normal(part)) if part == "assets" => {}
        _ => return None,
    }
    Some(canonicalize_or_clone(skill_dir.join(normalized)))
}

fn apply_config(skills: &mut [SkillMetadata], config: &SkillsConfigRequest) -> Result<()> {
    let normalized = normalize_config(config)?;
    for entry in normalized.config {
        match (entry.path, entry.name) {
            (Some(path), None) => {
                for skill in skills
                    .iter_mut()
                    .filter(|skill| skill.scope != SkillScope::Project && skill.path == path)
                {
                    skill.enabled = entry.enabled;
                }
            }
            (None, Some(name)) => {
                for skill in skills
                    .iter_mut()
                    .filter(|skill| skill.scope != SkillScope::Project && skill.name == name)
                {
                    skill.enabled = entry.enabled;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn build_injections_from_outcome(
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
            &name, &enabled, &blocked_plain_names, &BTreeSet::new(), &name_counts,
            false, &mut seen_paths, &mut result,
        );
    }

    for name in explicit_names {
        load_unique_skill(
            &name, &enabled, &blocked_plain_names, &input.reserved_names, &name_counts,
            false, &mut seen_paths, &mut result,
        );
    }

    for name in plain_text_names {
        load_unique_skill(
            &name, &enabled, &blocked_plain_names, &input.reserved_names, &name_counts,
            true, &mut seen_paths, &mut result,
        );
    }

    result
}

fn load_unique_skill(
    name: &str,
    enabled: &[&SkillMetadata],
    blocked: &BTreeSet<String>,
    reserved: &BTreeSet<String>,
    name_counts: &BTreeMap<String, usize>,
    require_implicit: bool,
    seen_paths: &mut BTreeSet<PathBuf>,
    result: &mut SkillInjections,
) {
    if blocked.contains(name)
        || reserved.contains(name)
        || name_counts.get(name).copied().unwrap_or_default() != 1
    {
        return;
    }
    if let Some(skill) = enabled.iter().find(|skill| skill.name == name) {
        if !require_implicit || skill_allows_implicit(skill) {
            load_skill_contents(skill, seen_paths, result);
        }
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

fn extract_plain_skill_names(text: &str, skills: &[&SkillMetadata]) -> BTreeSet<String> {
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

fn extract_frontmatter(contents: &str) -> Option<&str> {
    let rest = contents
        .strip_prefix("---\n")
        .or_else(|| contents.strip_prefix("---\r\n"))?;
    let marker = rest.find("\n---").or_else(|| rest.find("\r\n---"))?;
    Some(&rest[..marker])
}

fn required_single_line(
    value: Option<String>,
    field: &'static str,
) -> std::result::Result<String, SkillParseError> {
    let value = value.ok_or(SkillParseError::MissingField(field))?;
    let value = sanitize_single_line(&value);
    if value.is_empty() {
        Err(SkillParseError::MissingField(field))
    } else {
        Ok(value)
    }
}

fn optional_single_line(value: String, _field: &'static str) -> Option<String> {
    let value = sanitize_single_line(&value);
    (!value.is_empty()).then_some(value)
}

fn sanitize_single_line(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn validate_len(
    value: &str,
    max: usize,
    field: &'static str,
) -> std::result::Result<(), SkillParseError> {
    if value.chars().count() <= max {
        Ok(())
    } else {
        Err(SkillParseError::InvalidField {
            field,
            reason: format!("must be at most {max} characters"),
        })
    }
}

fn display_description(skill: &SkillMetadata) -> &str {
    skill
        .short_description
        .as_deref()
        .or_else(|| {
            skill
                .interface
                .as_ref()
                .and_then(|interface| interface.short_description.as_deref())
        })
        .unwrap_or(&skill.description)
}

fn skill_display_path(skill: &SkillMetadata) -> &Path {
    skill.source_path.as_deref().unwrap_or(&skill.path)
}

fn skill_sort(left: &SkillMetadata, right: &SkillMetadata) -> std::cmp::Ordering {
    scope_rank(left.scope)
        .cmp(&scope_rank(right.scope))
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| left.path.cmp(&right.path))
}

fn scope_rank(scope: SkillScope) -> u8 {
    match scope {
        SkillScope::Project => 0,
        SkillScope::Repo => 1,
        SkillScope::User => 2,
        SkillScope::System => 3,
    }
}

fn canonicalize_or_clone(path: impl AsRef<Path>) -> PathBuf {
    fs::canonicalize(path.as_ref()).unwrap_or_else(|_| path.as_ref().to_path_buf())
}

fn normalize_mention(value: &str) -> &str {
    value.trim().trim_start_matches('$')
}

fn looks_like_path(value: &str) -> bool {
    value.starts_with(SKILL_PATH_PREFIX)
        || value.contains('/')
        || value.contains('\\')
        || value.ends_with(SKILL_FILE)
}

fn normalized_skill_path(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    let normalized = raw.strip_prefix(SKILL_PATH_PREFIX).unwrap_or(&raw);
    canonicalize_or_clone(Path::new(normalized))
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discovers_skill_frontmatter_and_metadata() {
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("demo");
        fs::create_dir_all(skill_dir.join("agents")).expect("mkdir metadata");
        fs::create_dir_all(skill_dir.join("assets")).expect("mkdir assets");
        fs::write(skill_dir.join("assets/icon.png"), "icon").expect("icon");
        fs::write(
            skill_dir.join(SKILL_FILE),
            "---\nname: demo-skill\ndescription: Helps demo things.\nmetadata:\n  short-description: Short demo\n---\nbody",
        )
        .expect("write skill");
        fs::write(
            skill_dir.join("agents/openai.yaml"),
            "interface:\n  display_name: Demo\n  icon_small: assets/icon.png\n  default_prompt: Use demo\npolicy:\n  allow_implicit_invocation: false\ndependencies:\n  tools:\n    - type: mcp\n      value: filesystem\n",
        )
        .expect("write metadata");

        let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);
        let response = manager.list(&SkillsConfigRequest::default()).expect("list");
        assert!(response.errors.is_empty());
        assert_eq!(response.skills[0].name, "demo-skill");
        assert_eq!(
            response.skills[0].short_description.as_deref(),
            Some("Short demo")
        );
        assert_eq!(
            response.skills[0]
                .interface
                .as_ref()
                .and_then(|interface| interface.display_name.as_deref()),
            Some("Demo")
        );
        assert_eq!(
            response.skills[0]
                .dependencies
                .as_ref()
                .expect("dependencies")
                .tools[0]
                .kind,
            "mcp"
        );
        assert_eq!(
            response.skills[0]
                .policy
                .as_ref()
                .and_then(|policy| policy.allow_implicit_invocation),
            Some(false)
        );
    }

    #[test]
    fn invalid_skill_reports_error() {
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("bad");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(skill_dir.join(SKILL_FILE), "body").expect("write");

        let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);
        let response = manager.list(&SkillsConfigRequest::default()).expect("list");
        assert!(response.skills.is_empty());
        assert_eq!(response.errors.len(), 1);
    }

    #[test]
    fn discovers_skill_with_long_description() {
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("anthropic-like");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        let description = "Long description sentence. ".repeat(120);
        fs::write(
            skill_dir.join(SKILL_FILE),
            format!("---\nname: anthropic-like\ndescription: {description}\n---\nbody"),
        )
        .expect("write skill");

        let manager =
            SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::System)]);
        let response = manager.list(&SkillsConfigRequest::default()).expect("list");

        assert!(response.errors.is_empty());
        assert_eq!(response.skills.len(), 1);
        assert_eq!(response.skills[0].description, description.trim());
    }

    #[test]
    fn icon_paths_must_be_under_assets() {
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("demo");
        fs::create_dir_all(skill_dir.join("agents")).expect("mkdir metadata");
        fs::write(
            skill_dir.join(SKILL_FILE),
            "---\nname: demo\ndescription: Demo.\n---\nbody",
        )
        .expect("write skill");
        fs::write(
            skill_dir.join("agents/openai.yaml"),
            "interface:\n  icon_small: ../secret.png\n  icon_large: assets/icon.png\n",
        )
        .expect("write metadata");

        let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);
        let response = manager.list(&SkillsConfigRequest::default()).expect("list");
        let interface = response.skills[0].interface.as_ref().expect("interface");
        assert!(interface.icon_small.is_none());
        assert!(interface.icon_large.is_some());
    }

    #[test]
    fn root_order_and_dedupe_prefers_repo() {
        let dir = tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let project = dir.path().join("project");
        let user = dir.path().join("user");
        let system = dir.path().join("system");
        for root in [&repo, &project, &user, &system] {
            let skill_dir = root.join("demo");
            fs::create_dir_all(&skill_dir).expect("mkdir");
            fs::write(
                skill_dir.join(SKILL_FILE),
                "---\nname: demo\ndescription: Demo.\n---\nbody",
            )
            .expect("write");
        }
        let manager = SkillsManager::with_roots(vec![
            (system.clone(), SkillScope::System),
            (user, SkillScope::User),
            (repo.clone(), SkillScope::Repo),
            (project.clone(), SkillScope::Project),
        ]);
        let response = manager.list(&SkillsConfigRequest::default()).expect("list");
        assert_eq!(response.skills[0].scope, SkillScope::Project);
        assert!(response.skills[0].path.starts_with(project));
        assert_eq!(response.skills[1].scope, SkillScope::Repo);
        assert!(response.skills[1].path.starts_with(repo));
        assert_eq!(response.skills[2].scope, SkillScope::User);
        assert_eq!(response.skills[3].scope, SkillScope::System);
        assert!(response.skills[3].path.starts_with(system));
    }

    #[test]
    fn new_with_system_root_discovers_system_skills() {
        let dir = tempdir().expect("tempdir");
        let system = dir.path().join("system");
        let skill_dir = system.join("demo");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join(SKILL_FILE),
            "---\nname: system-demo\ndescription: System demo.\n---\nbody",
        )
        .expect("write");

        let manager = SkillsManager::new_with_system_root(dir.path(), Some(&system));
        let response = manager.list(&SkillsConfigRequest::default()).expect("list");

        let skill = response
            .skills
            .iter()
            .find(|skill| skill.name == "system-demo")
            .expect("system skill");
        assert_eq!(skill.scope, SkillScope::System);
        assert!(response.roots.iter().any(|root| root == &system));
    }

    #[test]
    fn new_with_system_root_and_extra_roots_discovers_project_skills() {
        let dir = tempdir().expect("tempdir");
        let system = dir.path().join("system");
        let project = dir.path().join("project");
        for (root, name) in [(&system, "system-demo"), (&project, "project-demo")] {
            let skill_dir = root.join(name);
            fs::create_dir_all(&skill_dir).expect("mkdir");
            fs::write(
                skill_dir.join(SKILL_FILE),
                format!("---\nname: {name}\ndescription: {name}\n---\nbody"),
            )
            .expect("write");
        }

        let manager = SkillsManager::new_with_system_root_and_extra_roots(
            dir.path(),
            Some(&system),
            vec![(project.clone(), SkillScope::Project)],
        );
        let response = manager.list(&SkillsConfigRequest::default()).expect("list");

        let project_skill = response
            .skills
            .iter()
            .find(|skill| skill.name == "project-demo")
            .expect("project skill");
        assert_eq!(project_skill.scope, SkillScope::Project);
        assert!(project_skill.path.starts_with(project));
        assert!(
            response
                .skills
                .iter()
                .any(|skill| skill.name == "system-demo")
        );
    }

    #[test]
    fn config_disables_by_name_and_path() {
        let dir = tempdir().expect("tempdir");
        for name in ["one", "two"] {
            let skill_dir = dir.path().join(name);
            fs::create_dir_all(&skill_dir).expect("mkdir");
            fs::write(
                skill_dir.join(SKILL_FILE),
                format!("---\nname: {name}\ndescription: {name}\n---\nbody"),
            )
            .expect("write");
        }
        let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);
        let one_path = canonicalize_or_clone(dir.path().join("one/SKILL.md"));
        let response = manager
            .list(&SkillsConfigRequest {
                config: vec![
                    SkillConfigEntry {
                        name: Some("two".to_string()),
                        path: None,
                        enabled: false,
                    },
                    SkillConfigEntry {
                        name: None,
                        path: Some(one_path),
                        enabled: false,
                    },
                ],
            })
            .expect("list");
        assert!(response.skills.iter().all(|skill| !skill.enabled));
    }

    #[test]
    fn config_disables_system_skill_by_name_and_path() {
        let dir = tempdir().expect("tempdir");
        let system = dir.path().join("system");
        for name in ["one", "two"] {
            let skill_dir = system.join(name);
            fs::create_dir_all(&skill_dir).expect("mkdir");
            fs::write(
                skill_dir.join(SKILL_FILE),
                format!("---\nname: {name}\ndescription: {name}\n---\nbody"),
            )
            .expect("write");
        }
        let manager = SkillsManager::with_roots(vec![(system.clone(), SkillScope::System)]);
        let one_path = canonicalize_or_clone(system.join("one/SKILL.md"));
        let response = manager
            .list(&SkillsConfigRequest {
                config: vec![
                    SkillConfigEntry {
                        name: Some("two".to_string()),
                        path: None,
                        enabled: false,
                    },
                    SkillConfigEntry {
                        name: None,
                        path: Some(one_path),
                        enabled: false,
                    },
                ],
            })
            .expect("list");
        assert!(response.skills.iter().all(|skill| !skill.enabled));
    }

    #[test]
    fn global_config_does_not_disable_project_scoped_skills() {
        let dir = tempdir().expect("tempdir");
        let project = dir.path().join("project");
        let skill_dir = project.join("demo");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join(SKILL_FILE),
            "---\nname: demo\ndescription: Demo.\n---\nbody",
        )
        .expect("write");
        let manager = SkillsManager::with_roots(vec![(project, SkillScope::Project)]);
        let response = manager
            .list(&SkillsConfigRequest {
                config: vec![SkillConfigEntry {
                    name: Some("demo".to_string()),
                    path: None,
                    enabled: false,
                }],
            })
            .expect("list");

        assert!(response.skills[0].enabled);
    }

    #[test]
    fn explicit_path_beats_duplicate_plain_name() {
        let dir = tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let user = dir.path().join("user");
        for root in [&repo, &user] {
            let skill_dir = root.join("demo");
            fs::create_dir_all(&skill_dir).expect("mkdir");
            fs::write(
                skill_dir.join(SKILL_FILE),
                "---\nname: demo\ndescription: Demo.\n---\nbody",
            )
            .expect("write");
        }
        let manager = SkillsManager::with_roots(vec![
            (repo, SkillScope::Repo),
            (user.clone(), SkillScope::User),
        ]);
        let ambiguous = manager
            .build_injections(&["demo".to_string()], &SkillsConfigRequest::default())
            .expect("inject");
        assert!(ambiguous.items.is_empty());
        let user_path = canonicalize_or_clone(user.join("demo/SKILL.md"));
        let selected = manager
            .build_injections(
                &[user_path.display().to_string(), "demo".to_string()],
                &SkillsConfigRequest::default(),
            )
            .expect("inject");
        assert_eq!(selected.items.len(), 1);
        assert_eq!(selected.items[0].metadata.scope, SkillScope::User);
    }

    #[test]
    fn explicit_mentions_do_not_fuzzy_match_display_plugin_directory_or_abbreviation() {
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("doc-reviewer");
        fs::create_dir_all(skill_dir.join("agents")).expect("mkdir metadata");
        fs::write(
            skill_dir.join(SKILL_FILE),
            "---\nname: github:doc-review\ndescription: Review docs.\n---\nBody.",
        )
        .expect("write skill");
        fs::write(
            skill_dir.join("agents/openai.yaml"),
            "interface:\n  display_name: Documentation Review\n",
        )
        .expect("write metadata");
        let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);

        for mention in ["Documentation Review", "doc-reviewer", "dr"] {
            let injected = manager
                .build_injections(&[mention.to_string()], &SkillsConfigRequest::default())
                .expect("inject");
            assert!(injected.items.is_empty(), "mention {mention}");
        }
    }

    #[test]
    fn text_skill_mentions_require_exact_boundary() {
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("alpha-skill");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join(SKILL_FILE),
            "---\nname: alpha-skill\ndescription: Alpha.\n---\nBody.",
        )
        .expect("write skill");
        let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);

        let near_miss = manager
            .build_injections_for_message("$alpha-skillx", &[], &SkillsConfigRequest::default())
            .expect("near miss");
        assert!(near_miss.items.is_empty());

        let matched = manager
            .build_injections_for_message("$alpha-skill.", &[], &SkillsConfigRequest::default())
            .expect("match");
        assert_eq!(matched.items.len(), 1);
        assert_eq!(matched.items[0].metadata.name, "alpha-skill");
    }

    #[test]
    fn plain_text_skill_mentions_require_exact_boundary() {
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("alpha-skill");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join(SKILL_FILE),
            "---\nname: alpha-skill\ndescription: Alpha.\n---\nBody.",
        )
        .expect("write skill");
        let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);

        let near_miss = manager
            .build_injections_for_message(
                "please use alpha-skillx",
                &[],
                &SkillsConfigRequest::default(),
            )
            .expect("near miss");
        assert!(near_miss.items.is_empty());

        let matched = manager
            .build_injections_for_message(
                "please use alpha-skill, thanks",
                &[],
                &SkillsConfigRequest::default(),
            )
            .expect("match");
        assert_eq!(matched.items.len(), 1);
        assert_eq!(matched.items[0].metadata.name, "alpha-skill");
    }

    #[test]
    fn linked_path_mentions_select_by_path_and_block_plain_fallback() {
        let dir = tempdir().expect("tempdir");
        let alpha = dir.path().join("alpha");
        let beta = dir.path().join("beta");
        for root in [&alpha, &beta] {
            let skill_dir = root.join("demo");
            fs::create_dir_all(&skill_dir).expect("mkdir");
            fs::write(
                skill_dir.join(SKILL_FILE),
                "---\nname: demo\ndescription: Demo.\n---\nBody.",
            )
            .expect("write skill");
        }
        let manager = SkillsManager::with_roots(vec![
            (alpha, SkillScope::Repo),
            (beta.clone(), SkillScope::User),
        ]);
        let beta_path = canonicalize_or_clone(beta.join("demo/SKILL.md"));

        let injected = manager
            .build_injections_for_message(
                &format!("use [$demo]({}) and $demo", beta_path.display()),
                &[],
                &SkillsConfigRequest::default(),
            )
            .expect("inject");
        assert_eq!(injected.items.len(), 1);
        assert_eq!(injected.items[0].metadata.scope, SkillScope::User);

        let missing = manager
            .build_injections_for_message(
                "use [$demo](/tmp/missing/SKILL.md) and $demo",
                &[],
                &SkillsConfigRequest::default(),
            )
            .expect("missing");
        assert!(missing.items.is_empty());
    }

    #[test]
    fn reserved_name_conflict_blocks_plain_name_but_not_path() {
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("demo");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join(SKILL_FILE),
            "---\nname: demo\ndescription: Demo.\n---\nBody.",
        )
        .expect("write skill");
        let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);
        let reserved_names = BTreeSet::from(["demo".to_string()]);

        let blocked = manager
            .build_injections_for_input(
                SkillInput {
                    text: Some("$demo"),
                    reserved_names: reserved_names.clone(),
                    ..Default::default()
                },
                &SkillsConfigRequest::default(),
            )
            .expect("blocked");
        assert!(blocked.items.is_empty());

        let plain_blocked = manager
            .build_injections_for_input(
                SkillInput {
                    text: Some("please use demo"),
                    reserved_names: reserved_names.clone(),
                    ..Default::default()
                },
                &SkillsConfigRequest::default(),
            )
            .expect("plain blocked");
        assert!(plain_blocked.items.is_empty());

        let path = canonicalize_or_clone(skill_dir.join(SKILL_FILE));
        let selected = manager
            .build_injections_for_input(
                SkillInput {
                    text: Some(&format!("use [$demo]({})", path.display())),
                    reserved_names,
                    ..Default::default()
                },
                &SkillsConfigRequest::default(),
            )
            .expect("path");
        assert_eq!(selected.items.len(), 1);
    }

    #[test]
    fn semantic_matches_are_available_but_not_runtime_auto_injected() {
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("frontend-app-builder");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join(SKILL_FILE),
            "---\nname: frontend-app-builder\ndescription: Build frontend apps.\n---\nFrontend body.",
        )
        .expect("write skill");
        let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);

        let injected = manager
            .build_injections_for_message(
                "please build a frontend app",
                &[],
                &SkillsConfigRequest::default(),
            )
            .expect("inject");
        assert!(injected.items.is_empty());
        assert!(injected.suggestions.is_empty());

        let available = manager
            .render_available(&SkillsConfigRequest::default())
            .expect("available");
        assert!(available.contains("task clearly matches a skill's description"));
    }

    #[test]
    fn implicit_policy_hides_from_available_but_explicit_can_inject() {
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("guarded");
        fs::create_dir_all(skill_dir.join("agents")).expect("mkdir metadata");
        fs::write(
            skill_dir.join(SKILL_FILE),
            "---\nname: guarded-skill\ndescription: Guarded skill.\n---\nGuarded body.",
        )
        .expect("write skill");
        fs::write(
            skill_dir.join("agents/openai.yaml"),
            "policy:\n  allow_implicit_invocation: false\n",
        )
        .expect("write metadata");
        let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);

        let available = manager
            .render_available(&SkillsConfigRequest::default())
            .expect("available");
        assert_eq!(available, "No skills are currently available.");

        let explicit = manager
            .build_injections_for_message(
                "please help",
                &["guarded-skill".to_string()],
                &SkillsConfigRequest::default(),
            )
            .expect("explicit");
        assert_eq!(explicit.items.len(), 1);

        let plain = manager
            .build_injections_for_message(
                "please use guarded-skill",
                &[],
                &SkillsConfigRequest::default(),
            )
            .expect("plain");
        assert!(plain.items.is_empty());

        let explicit_text = manager
            .build_injections_for_message(
                "please use $guarded-skill",
                &[],
                &SkillsConfigRequest::default(),
            )
            .expect("explicit text");
        assert_eq!(explicit_text.items.len(), 1);
    }

    #[test]
    fn extracts_skill_mentions_with_boundaries_and_env_skip() {
        assert_eq!(
            extract_skill_mentions("use $rust-dev, $plugin:skill and $PATH then $HOME"),
            vec!["rust-dev", "plugin:skill"]
        );
        assert_eq!(
            extract_skill_mentions("$alpha-skillx and $alpha-skill"),
            vec!["alpha-skillx", "alpha-skill"]
        );
    }
}
