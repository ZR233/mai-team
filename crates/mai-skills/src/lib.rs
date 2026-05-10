use mai_protocol::{
    SkillConfigEntry, SkillDependencies, SkillErrorInfo, SkillInterface, SkillMetadata,
    SkillPolicy, SkillScope, SkillToolDependency, SkillsConfigRequest, SkillsListResponse,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
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
const EXPLICIT_MATCH_THRESHOLD: i32 = 70;
const IMPLICIT_INJECT_THRESHOLD: i32 = 90;
const IMPLICIT_SUGGEST_THRESHOLD: i32 = 60;
const IMPLICIT_LEAD_THRESHOLD: i32 = 25;
const MAX_IMPLICIT_INJECTIONS: usize = 2;
const MAX_SKILL_SUGGESTIONS: usize = 3;

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
        let response = self.list(config)?;
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
            return Ok("No skills are currently available.".to_string());
        }

        skills.sort_by(skill_sort);
        let mut name_counts = BTreeMap::<String, usize>::new();
        for skill in &skills {
            *name_counts.entry(skill.name.clone()).or_default() += 1;
        }

        let mut lines = Vec::with_capacity(skills.len() + 3);
        lines.push("A skill is a set of local instructions to follow that is stored in a `SKILL.md` file. Below is the list of skills that can be used. Each entry includes a name, description, and file path so you can open the source for full instructions when using a specific skill.".to_string());
        lines.push("### Available Skills".to_string());
        for skill in skills {
            let duplicate = name_counts.get(&skill.name).copied().unwrap_or_default() > 1;
            let _ = duplicate;
            lines.push(format!(
                "- ${}: {} (path: {})",
                skill.name,
                display_description(&skill),
                skill.path.display()
            ));
        }
        lines.push("### How to use skills".to_string());
        lines.push("- Use a skill when the user names it with `$SkillName`, selects it from the UI, or the task clearly matches its description. Do not carry skills across turns unless re-mentioned.".to_string());
        lines.push("- After deciding to use a skill, read only the relevant parts of its `SKILL.md`; resolve relative references from that file's directory.".to_string());
        Ok(lines.join("\n"))
    }

    pub fn build_injections(
        &self,
        explicit_mentions: &[String],
        config: &SkillsConfigRequest,
    ) -> Result<SkillInjections> {
        let mut outcome = self.load_outcome();
        apply_config(&mut outcome.skills, config)?;
        Ok(build_injections_from_outcome(&outcome, explicit_mentions))
    }

    pub fn build_injections_for_message(
        &self,
        message: &str,
        explicit_mentions: &[String],
        config: &SkillsConfigRequest,
    ) -> Result<SkillInjections> {
        let mut outcome = self.load_outcome();
        apply_config(&mut outcome.skills, config)?;
        Ok(build_injections_for_message_from_outcome(
            &outcome,
            message,
            explicit_mentions,
        ))
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

pub fn extract_skill_mentions(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut seen = HashSet::<String>::new();
    let mut index = 0;
    while index < bytes.len() {
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
    explicit_mentions: &[String],
) -> SkillInjections {
    let mut result = SkillInjections::default();
    let mut seen_paths = BTreeSet::<PathBuf>::new();
    let mut blocked_plain_names = BTreeSet::<String>::new();
    let enabled = outcome
        .skills
        .iter()
        .filter(|skill| skill.enabled)
        .collect::<Vec<_>>();
    let name_counts = skill_name_counts(&enabled);

    for mention in explicit_mentions {
        let trimmed = normalize_mention(mention);
        if trimmed.is_empty() {
            continue;
        }
        if looks_like_path(trimmed) {
            let path = canonicalize_or_clone(Path::new(trimmed));
            blocked_plain_names.extend(
                enabled
                    .iter()
                    .filter(|skill| skill.path == path)
                    .map(|skill| skill.name.clone()),
            );
            if let Some(skill) = enabled.iter().find(|skill| skill.path == path) {
                load_skill_contents(skill, &mut seen_paths, &mut result);
            }
        }
    }

    for mention in explicit_mentions {
        let trimmed = normalize_mention(mention);
        if trimmed.is_empty() || looks_like_path(trimmed) || blocked_plain_names.contains(trimmed) {
            continue;
        }
        if name_counts.get(trimmed).copied().unwrap_or_default() != 1 {
            continue;
        }
        if let Some(skill) = enabled.iter().find(|skill| skill.name == trimmed) {
            load_skill_contents(skill, &mut seen_paths, &mut result);
        }
    }

    for mention in explicit_mentions {
        let trimmed = normalize_mention(mention);
        if trimmed.is_empty() || looks_like_path(trimmed) || blocked_plain_names.contains(trimmed) {
            continue;
        }
        if result
            .items
            .iter()
            .any(|skill| skill.metadata.name == trimmed)
        {
            continue;
        }
        if let Some(skill) = unique_explicit_skill_match(trimmed, &enabled, &seen_paths) {
            load_skill_contents(skill, &mut seen_paths, &mut result);
        }
    }

    result
}

fn build_injections_for_message_from_outcome(
    outcome: &SkillLoadOutcome,
    message: &str,
    explicit_mentions: &[String],
) -> SkillInjections {
    let mut mentions = explicit_mentions.to_vec();
    mentions.extend(extract_skill_mentions(message));
    let mut result = build_injections_from_outcome(outcome, &mentions);
    add_implicit_matches(outcome, message, &mut result);
    result
}

fn unique_explicit_skill_match<'a>(
    mention: &str,
    skills: &[&'a SkillMetadata],
    seen_paths: &BTreeSet<PathBuf>,
) -> Option<&'a SkillMetadata> {
    let mut matches = skills
        .iter()
        .copied()
        .filter(|skill| !seen_paths.contains(&skill.path))
        .filter_map(|skill| explicit_skill_match_score(skill, mention).map(|score| (score, skill)))
        .filter(|(score, _)| *score >= EXPLICIT_MATCH_THRESHOLD)
        .collect::<Vec<_>>();
    matches.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| skill_sort(left, right))
    });
    let (best_score, best_skill) = matches.first().copied()?;
    if matches
        .get(1)
        .is_some_and(|(score, _)| *score == best_score)
    {
        return None;
    }
    Some(best_skill)
}

#[derive(Debug, Clone)]
struct SkillCandidate<'a> {
    skill: &'a SkillMetadata,
    score: i32,
    reason: String,
}

fn add_implicit_matches(outcome: &SkillLoadOutcome, message: &str, result: &mut SkillInjections) {
    if message.trim().is_empty() {
        return;
    }
    let seen_paths = result
        .items
        .iter()
        .map(|skill| skill.metadata.path.clone())
        .collect::<BTreeSet<_>>();
    let mut candidates = outcome
        .skills
        .iter()
        .filter(|skill| {
            skill.enabled && skill_allows_implicit(skill) && !seen_paths.contains(&skill.path)
        })
        .filter_map(|skill| implicit_skill_match_score(skill, message))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| skill_sort(left.skill, right.skill))
    });

    let mut auto_paths = BTreeSet::<PathBuf>::new();
    for index in 0..candidates.len().min(MAX_IMPLICIT_INJECTIONS) {
        let candidate = &candidates[index];
        if candidate.score < IMPLICIT_INJECT_THRESHOLD {
            break;
        }
        let next_score = candidates
            .get(index + 1)
            .map(|item| item.score)
            .unwrap_or(0);
        if candidate.score - next_score < IMPLICIT_LEAD_THRESHOLD {
            break;
        }
        load_skill_contents(candidate.skill, &mut auto_paths, result);
    }

    let injected_paths = result
        .items
        .iter()
        .map(|skill| skill.metadata.path.clone())
        .collect::<BTreeSet<_>>();
    result.suggestions = candidates
        .into_iter()
        .filter(|candidate| {
            candidate.score >= IMPLICIT_SUGGEST_THRESHOLD
                && !injected_paths.contains(&candidate.skill.path)
        })
        .take(MAX_SKILL_SUGGESTIONS)
        .map(skill_suggestion)
        .collect();
}

fn skill_suggestion(candidate: SkillCandidate<'_>) -> SkillSuggestion {
    SkillSuggestion {
        name: candidate.skill.name.clone(),
        display_name: skill_display_name(candidate.skill),
        description: display_description(candidate.skill).to_string(),
        path: candidate.skill.path.clone(),
        scope: candidate.skill.scope,
        reason: candidate.reason,
        score: candidate.score,
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

fn explicit_skill_match_score(skill: &SkillMetadata, mention: &str) -> Option<i32> {
    let mention = normalize_token(mention);
    if mention.is_empty() {
        return None;
    }
    skill_aliases(skill)
        .into_iter()
        .filter_map(|alias| alias_match_score(&alias, &mention))
        .max()
}

fn implicit_skill_match_score<'a>(
    skill: &'a SkillMetadata,
    message: &str,
) -> Option<SkillCandidate<'a>> {
    let message_tokens = tokenize_for_match(message);
    if message_tokens.is_empty() {
        return None;
    }
    let message_norm = message_tokens.join(" ");
    let mut best: Option<(i32, String)> = None;
    for alias in skill_aliases(skill) {
        let alias_tokens = tokenize_for_match(&alias);
        if alias_tokens.is_empty() {
            continue;
        }
        let alias_norm = alias_tokens.join(" ");
        let score = if message_norm == alias_norm {
            115
        } else if contains_word_sequence(&message_tokens, &alias_tokens) {
            110
        } else if alias_tokens.iter().all(|token| {
            message_tokens
                .iter()
                .any(|message_token| message_token == token)
        }) {
            100
        } else {
            message_tokens
                .iter()
                .filter_map(|token| alias_match_score(&alias_norm, token))
                .max()
                .unwrap_or(0)
                .saturating_sub(5)
        };
        if score > best.as_ref().map(|(score, _)| *score).unwrap_or_default() {
            best = Some((score, format!("matched alias `{alias}`")));
        }
    }

    let description_tokens = tokenize_for_match(display_description(skill));
    if !description_tokens.is_empty() {
        let overlap = message_tokens
            .iter()
            .filter(|token| {
                token.len() >= 4 && description_tokens.iter().any(|desc| desc == *token)
            })
            .count();
        let score = match overlap {
            0 => 0,
            1 => 55,
            2 => 75,
            _ => 92 + (overlap.min(6) as i32 - 3) * 2,
        };
        if score > best.as_ref().map(|(score, _)| *score).unwrap_or_default() {
            best = Some((score, format!("matched {overlap} description keywords")));
        }
    }

    let (score, reason) = best?;
    (score >= IMPLICIT_SUGGEST_THRESHOLD).then_some(SkillCandidate {
        skill,
        score,
        reason,
    })
}

fn skill_aliases(skill: &SkillMetadata) -> Vec<String> {
    let mut aliases = Vec::new();
    push_alias(&mut aliases, &skill.name);
    if let Some((_plugin, name)) = skill.name.split_once(':') {
        push_alias(&mut aliases, name);
    }
    if let Some(display_name) = skill_display_name(skill) {
        push_alias(&mut aliases, &display_name);
    }
    if let Some(parent) = skill
        .path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
    {
        push_alias(&mut aliases, parent);
    }
    let current = aliases.clone();
    for alias in current {
        let abbreviation = alias_abbreviation(&alias);
        push_alias(&mut aliases, &abbreviation);
    }
    aliases
}

fn push_alias(aliases: &mut Vec<String>, value: &str) {
    let normalized = normalize_token(value);
    if !normalized.is_empty() && !aliases.contains(&normalized) {
        aliases.push(normalized);
    }
}

fn skill_display_name(skill: &SkillMetadata) -> Option<String> {
    skill
        .interface
        .as_ref()
        .and_then(|interface| interface.display_name.as_deref())
        .map(ToOwned::to_owned)
}

fn alias_abbreviation(value: &str) -> String {
    tokenize_for_match(value)
        .into_iter()
        .filter_map(|token| token.chars().next())
        .collect()
}

fn alias_match_score(alias: &str, needle: &str) -> Option<i32> {
    let alias = normalize_token(alias);
    let needle = normalize_token(needle);
    if alias.is_empty() || needle.is_empty() {
        return None;
    }
    if alias == needle {
        return Some(120);
    }
    if alias.starts_with(&needle) {
        return Some(105);
    }
    if alias
        .split_whitespace()
        .any(|part| part == needle || part.starts_with(&needle))
    {
        return Some(100);
    }
    if alias.contains(&needle) {
        return Some(90);
    }
    fuzzy_match_score(&alias, &needle).map(|score| score.saturating_sub(10))
}

fn fuzzy_match_score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return None;
    }
    let haystack_chars = haystack.chars().collect::<Vec<_>>();
    let needle_chars = needle.chars().collect::<Vec<_>>();
    let mut matched_positions = Vec::with_capacity(needle_chars.len());
    let mut cursor = 0usize;
    for needle_char in needle_chars {
        let mut found = None;
        while cursor < haystack_chars.len() {
            if haystack_chars[cursor] == needle_char {
                found = Some(cursor);
                cursor += 1;
                break;
            }
            cursor += 1;
        }
        matched_positions.push(found?);
    }
    let first = *matched_positions.first()?;
    let last = *matched_positions.last()?;
    let span = last.saturating_sub(first) + 1;
    let gaps = span.saturating_sub(matched_positions.len());
    let prefix_bonus = if first == 0 { 20 } else { 0 };
    let boundary_bonus = if first == 0
        || haystack_chars
            .get(first.saturating_sub(1))
            .is_some_and(|ch| !ch.is_alphanumeric())
    {
        10
    } else {
        0
    };
    Some((80 + prefix_bonus + boundary_bonus - gaps as i32).clamp(0, 100))
}

fn tokenize_for_match(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter_map(|part| {
            let token = normalize_token(part);
            (!token.is_empty() && !is_stop_word(&token)).then_some(token)
        })
        .collect()
}

fn contains_word_sequence(haystack: &[String], needle: &[String]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn normalize_token(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('$')
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn skill_allows_implicit(skill: &SkillMetadata) -> bool {
    skill
        .policy
        .as_ref()
        .and_then(|policy| policy.allow_implicit_invocation)
        .unwrap_or(true)
}

fn is_stop_word(value: &str) -> bool {
    matches!(
        value,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "by"
            | "for"
            | "from"
            | "help"
            | "i"
            | "in"
            | "is"
            | "it"
            | "me"
            | "of"
            | "on"
            | "or"
            | "please"
            | "the"
            | "this"
            | "to"
            | "use"
            | "with"
            | "you"
    )
}

fn skill_name_counts(skills: &[&SkillMetadata]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for skill in skills {
        *counts.entry(skill.name.clone()).or_default() += 1;
    }
    counts
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
    value.contains('/') || value.contains('\\') || value.ends_with(SKILL_FILE)
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
    fn explicit_fuzzy_matches_display_plugin_directory_and_abbreviation() {
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
            assert_eq!(injected.items.len(), 1, "mention {mention}");
            assert_eq!(injected.items[0].metadata.name, "github:doc-review");
        }
    }

    #[test]
    fn explicit_fuzzy_ambiguous_same_score_does_not_inject() {
        let dir = tempdir().expect("tempdir");
        for name in ["alpha-review", "alpha-runner"] {
            let skill_dir = dir.path().join(name);
            fs::create_dir_all(&skill_dir).expect("mkdir");
            fs::write(
                skill_dir.join(SKILL_FILE),
                format!("---\nname: {name}\ndescription: {name}\n---\nBody."),
            )
            .expect("write skill");
        }
        let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);

        let injected = manager
            .build_injections(&["alpha".to_string()], &SkillsConfigRequest::default())
            .expect("inject");

        assert!(injected.items.is_empty());
    }

    #[test]
    fn implicit_message_injects_high_confidence_and_suggests_lower_confidence() {
        let dir = tempdir().expect("tempdir");
        for (name, description, body) in [
            (
                "frontend-app-builder",
                "Build frontend apps.",
                "Frontend body.",
            ),
            (
                "release-notes",
                "Prepare changelog release summary migration notes.",
                "Release body.",
            ),
        ] {
            let skill_dir = dir.path().join(name);
            fs::create_dir_all(&skill_dir).expect("mkdir");
            fs::write(
                skill_dir.join(SKILL_FILE),
                format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
            )
            .expect("write skill");
        }
        let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);

        let injected = manager
            .build_injections_for_message(
                "please use the frontend app builder",
                &[],
                &SkillsConfigRequest::default(),
            )
            .expect("inject");
        assert_eq!(injected.items.len(), 1);
        assert_eq!(injected.items[0].metadata.name, "frontend-app-builder");

        let suggested = manager
            .build_injections_for_message("changelog summary", &[], &SkillsConfigRequest::default())
            .expect("suggest");
        assert!(suggested.items.is_empty());
        assert_eq!(suggested.suggestions.len(), 1);
        assert_eq!(suggested.suggestions[0].name, "release-notes");
    }

    #[test]
    fn implicit_policy_blocks_auto_but_explicit_can_inject() {
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

        let implicit = manager
            .build_injections_for_message(
                "please use guarded skill",
                &[],
                &SkillsConfigRequest::default(),
            )
            .expect("implicit");
        assert!(implicit.items.is_empty());
        assert!(implicit.suggestions.is_empty());

        let explicit = manager
            .build_injections_for_message(
                "please help",
                &["guarded-skill".to_string()],
                &SkillsConfigRequest::default(),
            )
            .expect("explicit");
        assert_eq!(explicit.items.len(), 1);
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
