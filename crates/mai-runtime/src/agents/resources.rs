use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};

use crate::skills::SkillsManager;
use mai_protocol::{SkillScope, SkillsConfigRequest, SkillsListResponse};
use serde_json::{Value, json};
use tokio::sync::OwnedRwLockReadGuard;

use crate::state::AgentRecord;
use crate::{Result, RuntimeError};

pub(crate) const SKILL_RESOURCE_SERVER: &str = "skill";
pub(crate) const PROJECT_SKILL_RESOURCE_SERVER: &str = "project-skill";
pub(crate) const SKILL_RESOURCE_SCHEME: &str = "skill:///";

pub(crate) struct AgentResourceBroker {
    pub(crate) skills: SkillsListResponse,
    pub(crate) _project_skill_guard: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
}

/// Provides MCP and skill resources needed to build an agent resource broker
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
    pub(crate) async fn list_resources(
        &self,
        server: Option<&str>,
        cursor: Option<String>,
    ) -> Result<Value> {
        if cursor.is_some() && is_skill_resource_server(server) {
            return Ok(json!({
                "server": server,
                "resources": [],
                "nextCursor": null,
            }));
        }
        if is_skill_resource_server(server) {
            return Ok(skill_resources_value(server, &self.skills.skills));
        }
        if let Some(server) = server {
            return Err(resource_provider_not_found(server));
        }
        Ok(json!({ "resources": skill_resource_values(&self.skills.skills) }))
    }

    pub(crate) async fn list_resource_templates(
        &self,
        server: Option<&str>,
        _cursor: Option<String>,
    ) -> Result<Value> {
        if is_skill_resource_server(server) {
            return Ok(json!({
                "server": server,
                "resourceTemplates": [],
                "nextCursor": null,
            }));
        }
        if let Some(server) = server {
            return Err(resource_provider_not_found(server));
        }
        Ok(json!({ "resourceTemplates": [] }))
    }

    pub(crate) async fn read_resource(&self, server: &str, uri: &str) -> Result<Value> {
        if is_skill_resource_server(Some(server)) || uri.starts_with(SKILL_RESOURCE_SCHEME) {
            return self.read_skill_resource(uri);
        }
        Err(resource_provider_not_found(server))
    }

    fn read_skill_resource(&self, uri: &str) -> Result<Value> {
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
            "contents": [{
                "uri": uri,
                "mimeType": skill_resource_mime_type(&path),
                "text": contents,
            }]
        }))
    }
}

fn skill_resources_value(server: Option<&str>, skills: &[mai_protocol::SkillMetadata]) -> Value {
    json!({
        "server": server,
        "resources": skill_resource_values(skills),
        "nextCursor": null,
    })
}

fn skill_resource_values(skills: &[mai_protocol::SkillMetadata]) -> Vec<Value> {
    skills
        .iter()
        .filter(|skill| skill.enabled)
        .map(|skill| {
            json!({
                "server": skill_resource_server_for_scope(skill.scope),
                "uri": skill_uri(&skill.name),
                "name": skill.name,
                "description": skill.description,
                "mimeType": "text/markdown",
            })
        })
        .collect()
}

fn skill_resource_server_for_scope(scope: SkillScope) -> &'static str {
    if scope == SkillScope::Project {
        PROJECT_SKILL_RESOURCE_SERVER
    } else {
        SKILL_RESOURCE_SERVER
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

fn is_skill_resource_server(server: Option<&str>) -> bool {
    server.is_some_and(|server| {
        server == SKILL_RESOURCE_SERVER
            || server == PROJECT_SKILL_RESOURCE_SERVER
            || server == format!("mcp:{SKILL_RESOURCE_SERVER}")
            || server == format!("mcp:{PROJECT_SKILL_RESOURCE_SERVER}")
    })
}

fn resource_provider_not_found(server: &str) -> RuntimeError {
    RuntimeError::InvalidInput(format!("resource provider not found: {server}"))
}
