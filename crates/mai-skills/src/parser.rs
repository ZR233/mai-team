use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use gray_matter::{Matter, engine::YAML};
use mai_protocol::{
    SkillDependencies, SkillInterface, SkillMetadata, SkillPolicy, SkillScope, SkillToolDependency,
};
use serde::Deserialize;

use crate::constants::{MAX_DESCRIPTION_LEN, MAX_NAME_LEN, METADATA_DIR, METADATA_FILE};
use crate::paths::canonicalize_or_clone;

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
pub(crate) enum SkillParseError {
    Read(std::io::Error),
    MissingFrontmatter,
    InvalidFrontmatter(gray_matter::Error),
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
            SkillParseError::InvalidFrontmatter(err) => write!(f, "invalid frontmatter: {err}"),
            SkillParseError::MissingField(field) => write!(f, "missing field `{field}`"),
            SkillParseError::InvalidField { field, reason } => {
                write!(f, "invalid {field}: {reason}")
            }
        }
    }
}

impl Error for SkillParseError {}

pub(crate) fn parse_skill_file(
    path: &Path,
    scope: SkillScope,
) -> std::result::Result<SkillMetadata, SkillParseError> {
    let contents = fs::read_to_string(path).map_err(SkillParseError::Read)?;
    let parsed = Matter::<YAML>::new()
        .parse::<SkillFrontmatter>(&contents)
        .map_err(SkillParseError::InvalidFrontmatter)?
        .data
        .ok_or(SkillParseError::MissingFrontmatter)?;
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
    let Ok(parsed) = serde_saphyr::from_str::<SkillMetadataFile>(&contents) else {
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
