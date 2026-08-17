use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};

use crate::skills::SkillsManager;
use mai_protocol::{SkillScope, SkillsConfigRequest, SkillsListResponse};
use serde_json::{Value, json};
use tokio::sync::OwnedRwLockReadGuard;

use crate::state::AgentRecord;
use crate::{Result, RuntimeError};

pub(crate) const SKILL_RESOURCE_SCHEME: &str = "skill:///";

pub(crate) struct AgentResourceBroker {
    pub(crate) skills: SkillsListResponse,
    pub(crate) _project_skill_guard: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
}

/// Provides the skill catalog needed to build an agent resource broker
/// without exposing the runtime facade to resource listing code.
pub(crate) trait AgentResourceBrokerOps: Send + Sync {
    fn project_skill_read_guard(
        &self,
        agent: &AgentRecord,
    ) -> impl Future<Output = Option<OwnedRwLockReadGuard<()>>> + Send;

    fn skills_config(&self) -> impl Future<Output = Result<SkillsConfigRequest>> + Send;

    fn skills_manager_for_agent(
        &self,
        agent: &AgentRecord,
    ) -> impl Future<Output = Result<SkillsManager>> + Send;
}

pub(crate) async fn agent_resource_broker(
    ops: &impl AgentResourceBrokerOps,
    agent: &AgentRecord,
) -> Result<AgentResourceBroker> {
    let project_skill_guard = ops.project_skill_read_guard(agent).await;
    let skills_config = ops.skills_config().await?;
    let skills = ops
        .skills_manager_for_agent(agent)
        .await?
        .list(&skills_config)?;
    Ok(AgentResourceBroker {
        skills,
        _project_skill_guard: project_skill_guard,
    })
}

impl AgentResourceBroker {
    /// 列出当前 agent 可用的技能资源。
    pub(crate) fn list_skill_resources(&self) -> Value {
        json!({ "resources": skill_resource_values(&self.skills.skills) })
    }

    /// 读取 `skill:///<skill-name>[/relative]` 指向的技能资源。
    pub(crate) fn read_skill_resource(&self, uri: &str) -> Result<Value> {
        let Some(resource) = uri.strip_prefix(SKILL_RESOURCE_SCHEME) else {
            return Err(RuntimeError::InvalidInput(format!(
                "invalid skill resource uri `{uri}`; expected skill:///<skill-name>"
            )));
        };
        let resource = resource.trim_start_matches('/');
        let (name, relative) = resource.split_once('/').unwrap_or((resource, ""));
        let name = name.trim();
        if name.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "skill resource uri must include a skill name".to_string(),
            ));
        }
        let matches = self
            .skills
            .skills
            .iter()
            .filter(|skill| skill.enabled && skill.name == name)
            .collect::<Vec<_>>();
        let skill = match matches.as_slice() {
            [skill] => *skill,
            [] => {
                return Err(RuntimeError::InvalidInput(format!(
                    "skill resource not found: {uri}"
                )));
            }
            _ => {
                return Err(RuntimeError::InvalidInput(format!(
                    "ambiguous skill resource `{name}`; select a specific skill path"
                )));
            }
        };
        let path = if relative.is_empty() {
            skill.path.clone()
        } else {
            let relative = safe_skill_resource_relative_path(relative)?;
            let Some(skill_dir) = skill.path.parent() else {
                return Err(RuntimeError::InvalidInput(format!(
                    "skill resource has no parent directory: {uri}"
                )));
            };
            skill_dir.join(relative)
        };
        let contents = fs::read_to_string(&path)?;
        Ok(json!({
            "uri": uri,
            "mimeType": skill_resource_mime_type(&path),
            "text": contents,
        }))
    }
}

fn skill_resource_values(skills: &[mai_protocol::SkillMetadata]) -> Vec<Value> {
    skills
        .iter()
        .filter(|skill| skill.enabled)
        .map(|skill| {
            json!({
                "uri": skill_uri(&skill.name),
                "name": skill.name,
                "description": skill.description,
                "scope": skill_resource_scope(skill.scope),
                "mimeType": "text/markdown",
            })
        })
        .collect()
}

fn skill_resource_scope(scope: SkillScope) -> &'static str {
    if scope == SkillScope::Project {
        "project"
    } else {
        "user"
    }
}

fn skill_uri(name: &str) -> String {
    format!("{SKILL_RESOURCE_SCHEME}{name}")
}

fn safe_skill_resource_relative_path(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::CurDir => {}
            _ => {
                return Err(RuntimeError::InvalidInput(
                    "skill resource relative path cannot be absolute or contain parent components"
                        .to_string(),
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "skill resource relative path cannot be empty".to_string(),
        ));
    }
    Ok(normalized)
}

fn skill_resource_mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("md") => "text/markdown",
        Some("py") => "text/x-python",
        Some("sh") => "text/x-shellscript",
        Some("json") => "application/json",
        Some("yaml" | "yml") => "application/yaml",
        _ => "text/plain",
    }
}
