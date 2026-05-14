use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mai_mcp::McpAgentManager;
use mai_protocol::{SkillScope, SkillsListResponse};
use serde_json::{Value, json};

use crate::{Result, RuntimeError};

pub(crate) const SKILL_RESOURCE_SERVER: &str = "skill";
pub(crate) const PROJECT_SKILL_RESOURCE_SERVER: &str = "project-skill";
pub(crate) const SKILL_RESOURCE_SCHEME: &str = "skill:///";

pub(crate) struct AgentResourceBroker {
    pub(crate) agent_mcp: Option<Arc<McpAgentManager>>,
    pub(crate) project_mcp: Option<Arc<McpAgentManager>>,
    pub(crate) skills: SkillsListResponse,
    pub(crate) _project_skill_guard: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
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
            let normalized = normalize_mcp_resource_server(server);
            if let Some(manager) = self.manager_for_server(&normalized).await {
                return Ok(manager.list_resources(Some(&normalized), cursor).await?);
            }
            return Err(resource_provider_not_found(server));
        }

        let mut resources = Vec::new();
        resources.extend(skill_resource_values(&self.skills.skills));
        for manager in self.mcp_managers() {
            let value = manager.list_resources(None, None).await?;
            if let Some(items) = value.get("resources").and_then(Value::as_array) {
                resources.extend(items.iter().cloned());
            }
        }
        Ok(json!({ "resources": resources }))
    }

    pub(crate) async fn list_resource_templates(
        &self,
        server: Option<&str>,
        cursor: Option<String>,
    ) -> Result<Value> {
        if is_skill_resource_server(server) {
            return Ok(json!({
                "server": server,
                "resourceTemplates": [],
                "nextCursor": null,
            }));
        }
        if let Some(server) = server {
            let normalized = normalize_mcp_resource_server(server);
            if let Some(manager) = self.manager_for_server(&normalized).await {
                return Ok(manager
                    .list_resource_templates(Some(&normalized), cursor)
                    .await?);
            }
            return Err(resource_provider_not_found(server));
        }

        let mut templates = Vec::new();
        for manager in self.mcp_managers() {
            let value = manager.list_resource_templates(None, None).await?;
            if let Some(items) = value.get("resourceTemplates").and_then(Value::as_array) {
                templates.extend(items.iter().cloned());
            }
        }
        Ok(json!({ "resourceTemplates": templates }))
    }

    pub(crate) async fn read_resource(&self, server: &str, uri: &str) -> Result<Value> {
        if is_skill_resource_server(Some(server)) || uri.starts_with(SKILL_RESOURCE_SCHEME) {
            return self.read_skill_resource(uri);
        }
        let normalized = normalize_mcp_resource_server(server);
        if let Some(manager) = self.manager_for_server(&normalized).await {
            return Ok(manager.read_resource(&normalized, uri).await?);
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

    async fn manager_for_server(&self, server: &str) -> Option<Arc<McpAgentManager>> {
        for manager in self.mcp_managers() {
            if manager
                .resource_servers()
                .await
                .iter()
                .any(|resource_server| resource_server == server)
            {
                return Some(manager);
            }
        }
        None
    }

    fn mcp_managers(&self) -> Vec<Arc<McpAgentManager>> {
        let mut managers = Vec::new();
        if let Some(manager) = &self.agent_mcp {
            managers.push(Arc::clone(manager));
        }
        if let Some(manager) = &self.project_mcp
            && !managers
                .iter()
                .any(|existing| Arc::ptr_eq(existing, manager))
        {
            managers.push(Arc::clone(manager));
        }
        managers
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

fn normalize_mcp_resource_server(server: &str) -> Cow<'_, str> {
    server
        .strip_prefix("mcp:")
        .map(Cow::Borrowed)
        .unwrap_or_else(|| Cow::Borrowed(server))
}

fn resource_provider_not_found(server: &str) -> RuntimeError {
    RuntimeError::InvalidInput(format!("resource provider not found: {server}"))
}
