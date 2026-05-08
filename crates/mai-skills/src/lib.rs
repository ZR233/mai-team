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
const MAX_DESCRIPTION_LEN: usize = 1024;

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
    pub warnings: Vec<String>,
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
            if duplicate {
                lines.push(format!(
                    "- ${}: {} (path: {})",
                    skill.name,
                    display_description(&skill),
                    skill.path.display()
                ));
            } else {
                lines.push(format!(
                    "- ${}: {} (path: {})",
                    skill.name,
                    display_description(&skill),
                    skill.path.display()
                ));
            }
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
    Some(canonicalize_or_clone(&skill_dir.join(normalized)))
}

fn apply_config(skills: &mut [SkillMetadata], config: &SkillsConfigRequest) -> Result<()> {
    let normalized = normalize_config(config)?;
    for entry in normalized.config {
        match (entry.path, entry.name) {
            (Some(path), None) => {
                for skill in skills.iter_mut().filter(|skill| skill.path == path) {
                    skill.enabled = entry.enabled;
                }
            }
            (None, Some(name)) => {
                for skill in skills.iter_mut().filter(|skill| skill.name == name) {
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

    result
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
        SkillScope::Repo => 0,
        SkillScope::User => 1,
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
            (user, SkillScope::User),
            (repo.clone(), SkillScope::Repo),
        ]);
        let response = manager.list(&SkillsConfigRequest::default()).expect("list");
        assert_eq!(response.skills[0].scope, SkillScope::Repo);
        assert!(response.skills[0].path.starts_with(repo));
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
