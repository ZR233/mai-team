use mai_protocol::{
    AgentCapabilities, AgentProfile, AgentProfileErrorInfo, AgentProfileScope, AgentProfileSummary,
    AgentProfilesResponse,
};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use walkdir::{DirEntry, WalkDir};

const AGENT_FILE: &str = "AGENT.md";
const MAX_SCAN_DEPTH: usize = 6;
const MAX_ID_LEN: usize = 128;
const MAX_NAME_LEN: usize = 128;
const MAX_SLOT_LEN: usize = 128;
const MAX_DESCRIPTION_LEN: usize = 8192;
const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

#[derive(Debug, Error)]
pub enum AgentProfileError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

pub type Result<T> = std::result::Result<T, AgentProfileError>;

#[derive(Debug, Clone)]
pub struct AgentProfilesManager {
    roots: Vec<AgentProfileRoot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentProfileRoot {
    path: PathBuf,
    scope: AgentProfileScope,
}

#[derive(Debug, Clone, Default)]
struct AgentProfileLoadOutcome {
    roots: Vec<PathBuf>,
    profiles: Vec<AgentProfile>,
    errors: Vec<AgentProfileErrorInfo>,
}

#[derive(Debug, Deserialize)]
struct AgentFrontmatter {
    id: Option<String>,
    name: Option<String>,
    description: Option<String>,
    slot: Option<String>,
    version: Option<u64>,
    default_model_role: Option<String>,
    #[serde(default)]
    default_skills: Vec<String>,
    #[serde(default)]
    mcp_servers: Vec<String>,
    #[serde(default)]
    capabilities: AgentCapabilitiesFrontmatter,
}

#[derive(Debug, Default, Deserialize)]
struct AgentCapabilitiesFrontmatter {
    #[serde(default)]
    spawn_agents: bool,
    #[serde(default)]
    close_agents: bool,
    communication: Option<String>,
}

#[derive(Debug)]
enum AgentProfileParseError {
    Read(std::io::Error),
    MissingFrontmatter,
    InvalidYaml(serde_yaml::Error),
    MissingField(&'static str),
    InvalidField { field: &'static str, reason: String },
}

impl fmt::Display for AgentProfileParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentProfileParseError::Read(err) => write!(f, "failed to read file: {err}"),
            AgentProfileParseError::MissingFrontmatter => {
                write!(f, "missing YAML frontmatter delimited by ---")
            }
            AgentProfileParseError::InvalidYaml(err) => write!(f, "invalid YAML: {err}"),
            AgentProfileParseError::MissingField(field) => write!(f, "missing field `{field}`"),
            AgentProfileParseError::InvalidField { field, reason } => {
                write!(f, "invalid {field}: {reason}")
            }
        }
    }
}

impl Error for AgentProfileParseError {}

impl AgentProfilesManager {
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
        extra_roots: Vec<(PathBuf, AgentProfileScope)>,
    ) -> Self {
        let mut roots = default_roots_with_system(
            repo_root.as_ref(),
            system_root.as_ref().map(|path| path.as_ref()),
        );
        roots.extend(
            extra_roots
                .into_iter()
                .map(|(path, scope)| AgentProfileRoot { path, scope }),
        );
        Self { roots }
    }

    pub fn with_roots(roots: Vec<(PathBuf, AgentProfileScope)>) -> Self {
        Self {
            roots: roots
                .into_iter()
                .map(|(path, scope)| AgentProfileRoot { path, scope })
                .collect(),
        }
    }

    pub fn root_paths(&self) -> Vec<PathBuf> {
        self.roots.iter().map(|root| root.path.clone()).collect()
    }

    pub fn clone_with_extra_roots(&self, extra_roots: Vec<(PathBuf, AgentProfileScope)>) -> Self {
        let mut roots = self.roots.clone();
        roots.extend(
            extra_roots
                .into_iter()
                .map(|(path, scope)| AgentProfileRoot { path, scope }),
        );
        Self { roots }
    }

    pub fn list(&self) -> AgentProfilesResponse {
        let outcome = self.load_outcome();
        AgentProfilesResponse {
            roots: outcome.roots,
            profiles: outcome
                .profiles
                .iter()
                .map(AgentProfileSummary::from)
                .collect(),
            errors: outcome.errors,
        }
    }

    pub fn load_profiles(&self) -> Vec<AgentProfile> {
        self.load_outcome().profiles
    }

    pub fn load_profile(&self, id: &str) -> Option<AgentProfile> {
        self.load_outcome()
            .profiles
            .into_iter()
            .find(|profile| profile.id == id)
    }

    pub fn resolve_for_slot(
        &self,
        slot: &str,
        preferred_profile_id: Option<&str>,
    ) -> Option<AgentProfile> {
        let profiles = self.load_outcome().profiles;
        if let Some(profile_id) = preferred_profile_id
            && let Some(profile) = profiles
                .iter()
                .find(|profile| profile.enabled && profile.id == profile_id && profile.slot == slot)
        {
            return Some(profile.clone());
        }
        profiles
            .into_iter()
            .find(|profile| profile.enabled && profile.slot == slot)
    }

    fn load_outcome(&self) -> AgentProfileLoadOutcome {
        let mut outcome = AgentProfileLoadOutcome::default();
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
                if !entry.file_type().is_file() || entry.file_name() != AGENT_FILE {
                    continue;
                }
                let path = entry.path().to_path_buf();
                let canonical = canonicalize_or_clone(&path);
                if !seen_paths.insert(canonical.clone()) {
                    continue;
                }
                match parse_agent_file(&path, root.scope) {
                    Ok(mut profile) => {
                        profile.path = canonical;
                        outcome.profiles.push(profile);
                    }
                    Err(err) => outcome.errors.push(AgentProfileErrorInfo {
                        path: canonical,
                        message: err.to_string(),
                    }),
                }
            }
        }

        outcome.profiles.sort_by(agent_profile_sort);
        outcome
    }
}

fn default_roots(repo_root: &Path) -> Vec<AgentProfileRoot> {
    default_roots_with_system(repo_root, None)
}

fn default_roots_with_system(
    repo_root: &Path,
    system_root: Option<&Path>,
) -> Vec<AgentProfileRoot> {
    let mut roots = vec![AgentProfileRoot {
        path: repo_root.join(".agents").join("agents"),
        scope: AgentProfileScope::Repo,
    }];
    if let Some(home) = dirs::home_dir() {
        roots.push(AgentProfileRoot {
            path: home.join(".agents").join("agents"),
            scope: AgentProfileScope::User,
        });
    }
    if let Some(system_root) = system_root {
        roots.push(AgentProfileRoot {
            path: system_root.to_path_buf(),
            scope: AgentProfileScope::System,
        });
    }
    roots
}

fn include_entry(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    entry.depth() == 0 || !name.starts_with('.')
}

fn parse_agent_file(
    path: &Path,
    scope: AgentProfileScope,
) -> std::result::Result<AgentProfile, AgentProfileParseError> {
    let contents = fs::read_to_string(path).map_err(AgentProfileParseError::Read)?;
    let (frontmatter, prompt) =
        split_frontmatter(&contents).ok_or(AgentProfileParseError::MissingFrontmatter)?;
    let parsed: AgentFrontmatter =
        serde_yaml::from_str(frontmatter).map_err(AgentProfileParseError::InvalidYaml)?;
    let id = required_single_line(parsed.id, "id")?;
    let name = required_single_line(parsed.name, "name")?;
    let description = required_single_line(parsed.description, "description")?;
    let slot = required_single_line(parsed.slot, "slot")?;
    let version = parsed
        .version
        .ok_or(AgentProfileParseError::MissingField("version"))?;
    validate_len(&id, MAX_ID_LEN, "id")?;
    validate_len(&name, MAX_NAME_LEN, "name")?;
    validate_len(&description, MAX_DESCRIPTION_LEN, "description")?;
    validate_len(&slot, MAX_SLOT_LEN, "slot")?;
    validate_identifier(&id, "id")?;
    validate_slot(&slot)?;

    Ok(AgentProfile {
        id,
        name,
        description,
        slot,
        version,
        path: path.to_path_buf(),
        source_path: None,
        scope,
        enabled: true,
        prompt: prompt.trim().to_string(),
        default_model_role: parsed
            .default_model_role
            .and_then(|value| optional_single_line(value, "default_model_role")),
        default_skills: sanitize_list(parsed.default_skills, "default_skills")?,
        mcp_servers: sanitize_list(parsed.mcp_servers, "mcp_servers")?,
        capabilities: AgentCapabilities {
            spawn_agents: parsed.capabilities.spawn_agents,
            close_agents: parsed.capabilities.close_agents,
            communication: parsed
                .capabilities
                .communication
                .and_then(|value| optional_single_line(value, "capabilities.communication")),
        },
        hash: stable_hash(&contents),
    })
}

fn split_frontmatter(contents: &str) -> Option<(&str, &str)> {
    let rest = contents
        .strip_prefix("---\n")
        .or_else(|| contents.strip_prefix("---\r\n"))?;
    let marker_start = rest.find("\n---").or_else(|| rest.find("\r\n---"))?;
    let marker_end = marker_start
        + if rest[marker_start..].starts_with("\r\n---") {
            "\r\n---".len()
        } else {
            "\n---".len()
        };
    let prompt_start = if rest[marker_end..].starts_with("\r\n") {
        marker_end + 2
    } else if rest[marker_end..].starts_with('\n') {
        marker_end + 1
    } else {
        marker_end
    };
    Some((&rest[..marker_start], &rest[prompt_start..]))
}

fn required_single_line(
    value: Option<String>,
    field: &'static str,
) -> std::result::Result<String, AgentProfileParseError> {
    let value = value.ok_or(AgentProfileParseError::MissingField(field))?;
    let value = sanitize_single_line(&value);
    if value.is_empty() {
        Err(AgentProfileParseError::MissingField(field))
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

fn sanitize_list(
    values: Vec<String>,
    field: &'static str,
) -> std::result::Result<Vec<String>, AgentProfileParseError> {
    let mut out = Vec::new();
    for value in values {
        let value = sanitize_single_line(&value);
        if value.is_empty() {
            continue;
        }
        validate_len(&value, MAX_ID_LEN, field)?;
        out.push(value);
    }
    Ok(out)
}

fn validate_len(
    value: &str,
    max: usize,
    field: &'static str,
) -> std::result::Result<(), AgentProfileParseError> {
    if value.chars().count() <= max {
        Ok(())
    } else {
        Err(AgentProfileParseError::InvalidField {
            field,
            reason: format!("must be at most {max} characters"),
        })
    }
}

fn validate_identifier(
    value: &str,
    field: &'static str,
) -> std::result::Result<(), AgentProfileParseError> {
    if value
        .bytes()
        .all(|byte| matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b':'))
    {
        Ok(())
    } else {
        Err(AgentProfileParseError::InvalidField {
            field,
            reason: "must contain only letters, numbers, `_`, `-`, or `:`".to_string(),
        })
    }
}

fn validate_slot(value: &str) -> std::result::Result<(), AgentProfileParseError> {
    if value
        .bytes()
        .all(|byte| matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.'))
    {
        Ok(())
    } else {
        Err(AgentProfileParseError::InvalidField {
            field: "slot",
            reason: "must contain only letters, numbers, `_`, `-`, or `.`".to_string(),
        })
    }
}

fn stable_hash(contents: &str) -> String {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in contents.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

fn agent_profile_sort(left: &AgentProfile, right: &AgentProfile) -> std::cmp::Ordering {
    scope_rank(left.scope)
        .cmp(&scope_rank(right.scope))
        .then_with(|| left.slot.cmp(&right.slot))
        .then_with(|| left.id.cmp(&right.id))
        .then_with(|| left.path.cmp(&right.path))
}

fn scope_rank(scope: AgentProfileScope) -> u8 {
    match scope {
        AgentProfileScope::Project => 0,
        AgentProfileScope::Repo => 1,
        AgentProfileScope::User => 2,
        AgentProfileScope::System => 3,
    }
}

fn canonicalize_or_clone(path: impl AsRef<Path>) -> PathBuf {
    fs::canonicalize(path.as_ref()).unwrap_or_else(|_| path.as_ref().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discovers_agent_profile_frontmatter_and_prompt() {
        let dir = tempdir().expect("tempdir");
        let agent_dir = dir.path().join("project-reviewer");
        fs::create_dir_all(&agent_dir).expect("mkdir");
        fs::write(
            agent_dir.join(AGENT_FILE),
            "---\nid: project-reviewer\nname: Project Reviewer\ndescription: Reviews pull requests.\nslot: project.reviewer\nversion: 1\ndefault_model_role: reviewer\ndefault_skills:\n  - reviewer-agent-review-pr\nmcp_servers:\n  - github\ncapabilities:\n  communication: parent_and_maintainer\n---\n\nYou review PRs.\n",
        )
        .expect("write agent");

        let manager = AgentProfilesManager::with_roots(vec![(
            dir.path().to_path_buf(),
            AgentProfileScope::System,
        )]);
        let response = manager.list();
        assert!(response.errors.is_empty());
        assert_eq!(response.profiles.len(), 1);
        let profile = manager.load_profile("project-reviewer").expect("profile");
        assert_eq!(profile.name, "Project Reviewer");
        assert_eq!(profile.slot, "project.reviewer");
        assert_eq!(profile.version, 1);
        assert_eq!(profile.scope, AgentProfileScope::System);
        assert_eq!(profile.prompt, "You review PRs.");
        assert_eq!(profile.default_model_role.as_deref(), Some("reviewer"));
        assert_eq!(profile.default_skills, vec!["reviewer-agent-review-pr"]);
        assert_eq!(profile.mcp_servers, vec!["github"]);
        assert_eq!(
            profile.capabilities.communication.as_deref(),
            Some("parent_and_maintainer")
        );
        assert_eq!(profile.hash.len(), 16);
    }

    #[test]
    fn invalid_agent_reports_error() {
        let dir = tempdir().expect("tempdir");
        let agent_dir = dir.path().join("bad");
        fs::create_dir_all(&agent_dir).expect("mkdir");
        fs::write(agent_dir.join(AGENT_FILE), "body").expect("write");

        let manager = AgentProfilesManager::with_roots(vec![(
            dir.path().to_path_buf(),
            AgentProfileScope::Repo,
        )]);
        let response = manager.list();
        assert!(response.profiles.is_empty());
        assert_eq!(response.errors.len(), 1);
    }

    #[test]
    fn root_order_prefers_project_profiles() {
        let dir = tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let project = dir.path().join("project");
        let user = dir.path().join("user");
        let system = dir.path().join("system");
        for (root, id) in [
            (&system, "system-reviewer"),
            (&user, "user-reviewer"),
            (&repo, "repo-reviewer"),
            (&project, "project-reviewer"),
        ] {
            let agent_dir = root.join(id);
            fs::create_dir_all(&agent_dir).expect("mkdir");
            fs::write(
                agent_dir.join(AGENT_FILE),
                format!(
                    "---\nid: {id}\nname: {id}\ndescription: Demo.\nslot: project.reviewer\nversion: 1\n---\nbody"
                ),
            )
            .expect("write");
        }

        let manager = AgentProfilesManager::with_roots(vec![
            (system.clone(), AgentProfileScope::System),
            (user, AgentProfileScope::User),
            (repo.clone(), AgentProfileScope::Repo),
            (project.clone(), AgentProfileScope::Project),
        ]);
        let response = manager.list();

        assert_eq!(response.profiles[0].scope, AgentProfileScope::Project);
        assert!(response.profiles[0].path.starts_with(project));
        assert_eq!(response.profiles[1].scope, AgentProfileScope::Repo);
        assert!(response.profiles[1].path.starts_with(repo));
        assert_eq!(response.profiles[2].scope, AgentProfileScope::User);
        assert_eq!(response.profiles[3].scope, AgentProfileScope::System);
        assert!(response.profiles[3].path.starts_with(system));
    }

    #[test]
    fn new_with_system_root_discovers_system_agents() {
        let dir = tempdir().expect("tempdir");
        let system = dir.path().join("system-agents");
        let agent_dir = system.join("project-maintainer");
        fs::create_dir_all(&agent_dir).expect("mkdir");
        fs::write(
            agent_dir.join(AGENT_FILE),
            "---\nid: project-maintainer\nname: Project Maintainer\ndescription: Maintains projects.\nslot: project.maintainer\nversion: 1\n---\nbody",
        )
        .expect("write");

        let manager = AgentProfilesManager::new_with_system_root(dir.path(), Some(&system));
        let response = manager.list();

        let profile = response
            .profiles
            .iter()
            .find(|profile| profile.id == "project-maintainer")
            .expect("system profile");
        assert_eq!(profile.scope, AgentProfileScope::System);
        assert!(response.roots.iter().any(|root| root == &system));
    }

    #[test]
    fn resolves_bound_profile_or_first_slot_default() {
        let dir = tempdir().expect("tempdir");
        for id in ["project-maintainer", "rust-maintainer"] {
            let agent_dir = dir.path().join(id);
            fs::create_dir_all(&agent_dir).expect("mkdir");
            fs::write(
                agent_dir.join(AGENT_FILE),
                format!(
                    "---\nid: {id}\nname: {id}\ndescription: Demo.\nslot: project.maintainer\nversion: 1\n---\nbody"
                ),
            )
            .expect("write");
        }
        let manager = AgentProfilesManager::with_roots(vec![(
            dir.path().to_path_buf(),
            AgentProfileScope::Repo,
        )]);

        let bound = manager
            .resolve_for_slot("project.maintainer", Some("rust-maintainer"))
            .expect("bound");
        assert_eq!(bound.id, "rust-maintainer");

        let default = manager
            .resolve_for_slot("project.maintainer", Some("missing"))
            .expect("default");
        assert_eq!(default.id, "project-maintainer");
    }

    #[test]
    fn hash_changes_when_profile_file_changes() {
        let dir = tempdir().expect("tempdir");
        let agent_dir = dir.path().join("project-maintainer");
        fs::create_dir_all(&agent_dir).expect("mkdir");
        let path = agent_dir.join(AGENT_FILE);
        fs::write(
            &path,
            "---\nid: project-maintainer\nname: Project Maintainer\ndescription: Maintains projects.\nslot: project.maintainer\nversion: 1\n---\nfirst",
        )
        .expect("write first");
        let first = AgentProfilesManager::with_roots(vec![(
            dir.path().to_path_buf(),
            AgentProfileScope::Repo,
        )])
        .load_profile("project-maintainer")
        .expect("first")
        .hash;
        fs::write(
            &path,
            "---\nid: project-maintainer\nname: Project Maintainer\ndescription: Maintains projects.\nslot: project.maintainer\nversion: 1\n---\nsecond",
        )
        .expect("write second");
        let second = AgentProfilesManager::with_roots(vec![(
            dir.path().to_path_buf(),
            AgentProfileScope::Repo,
        )])
        .load_profile("project-maintainer")
        .expect("second")
        .hash;

        assert_ne!(first, second);
    }
}
