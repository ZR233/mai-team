use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mai_mcp::McpTool;
use mai_protocol::{ModelInputItem, SkillActivationInfo, SkillScope, SkillsListResponse};
use mai_skills::{SkillInjections, render_available_response};

pub(crate) const CONTAINER_SKILLS_ROOT: &str = "/tmp/.mai-team/skills";

const BASE_INSTRUCTIONS: &str = r#"You are Mai, a coding agent running inside a Docker-backed multi-agent service.

General rules:
- You execute all local work inside your own Docker container; do not assume access to a host workspace.
- Use `container_exec` for shell commands inside your container.
- Use `container_cp_upload` and `container_cp_download` for file transfer.
- Use `spawn_agent`, `send_input`, `wait_agent`, `list_agents`, `close_agent`, and `resume_agent` for multi-agent collaboration.
- Use `list_mcp_resources`, `list_mcp_resource_templates`, and `read_mcp_resource` to inspect MCP resources when available.
- Keep each child agent task concrete and bounded. Multiple agents can run in parallel.
- Child agent model selection is controlled by Research Agent settings, falling back to the service default model when unset.
- Use available skills only when explicitly requested by the user or when clearly relevant.
- MCP tools are exposed as ordinary function tools whose names begin with `mcp__`.
- Be concise with final answers and include important file paths or command outputs when they matter.
"#;

#[derive(Debug, Clone, Default)]
pub(crate) struct ContainerSkillPaths {
    paths: HashMap<PathBuf, PathBuf>,
}

impl ContainerSkillPaths {
    pub(crate) fn from_paths(paths: HashMap<PathBuf, PathBuf>) -> Self {
        Self { paths }
    }

    fn get(&self, path: &Path) -> Option<&PathBuf> {
        self.paths.get(path)
    }
}

pub(crate) fn build_instructions(
    system_prompt: Option<&str>,
    mut skills_response: SkillsListResponse,
    skill_injections: &SkillInjections,
    mcp_tools: &[McpTool],
    container_skill_paths: &ContainerSkillPaths,
    prefer_container_skill_paths: bool,
) -> String {
    let mut instructions = String::from(BASE_INSTRUCTIONS);
    if let Some(system_prompt) = system_prompt {
        instructions.push_str("\n\n## Agent System Prompt\n");
        instructions.push_str(system_prompt);
    }
    instructions.push_str("\n\n## Available Skills\n");
    apply_container_skill_paths(
        &mut skills_response,
        container_skill_paths,
        prefer_container_skill_paths,
    );
    instructions.push_str(&render_available_response(skills_response));
    if !skill_injections.warnings.is_empty() {
        instructions.push_str("\n\n## Skill Warnings\n");
        for warning in &skill_injections.warnings {
            instructions.push_str(&format!("\n- {warning}"));
        }
    }
    instructions.push_str("\n\n## MCP Tools\n");
    if mcp_tools.is_empty() {
        instructions.push_str("No MCP tools are currently available.");
    } else {
        for tool in mcp_tools {
            instructions.push_str(&format!(
                "\n- {} maps to MCP `{}` on server `{}`",
                tool.model_name, tool.name, tool.server
            ));
        }
    }
    instructions
}

pub(crate) fn skill_activation_info(
    skill_injections: &SkillInjections,
    container_skill_paths: &ContainerSkillPaths,
) -> Vec<SkillActivationInfo> {
    skill_injections
        .items
        .iter()
        .map(|skill| SkillActivationInfo {
            name: skill.metadata.name.clone(),
            display_name: skill
                .metadata
                .interface
                .as_ref()
                .and_then(|interface| interface.display_name.clone()),
            path: display_skill_path(&skill.metadata.path, container_skill_paths),
            scope: skill.metadata.scope,
        })
        .collect()
}

pub(crate) fn skill_user_fragment(
    skill_injections: &SkillInjections,
    container_skill_paths: &ContainerSkillPaths,
) -> Option<ModelInputItem> {
    if skill_injections.items.is_empty() {
        return None;
    }
    let mut text = String::new();
    for skill in &skill_injections.items {
        let path = display_skill_path(&skill.metadata.path, container_skill_paths);
        text.push_str(&format!(
            "\n<skill>\n<name>{}</name>\n<path>{}</path>\n{}\n</skill>\n",
            skill.metadata.name,
            path.display(),
            skill.contents
        ));
    }
    Some(ModelInputItem::user_text(text))
}

pub(crate) fn container_skill_dir(skill: &mai_protocol::SkillMetadata) -> PathBuf {
    let scope = match skill.scope {
        SkillScope::System => "system",
        SkillScope::Project => "project",
        SkillScope::Repo => "repo",
        SkillScope::User => "user",
    };
    PathBuf::from(CONTAINER_SKILLS_ROOT)
        .join(scope)
        .join(safe_container_skill_segment(&skill.name))
}

fn apply_container_skill_paths(
    response: &mut SkillsListResponse,
    container_skill_paths: &ContainerSkillPaths,
    overwrite_existing_source: bool,
) {
    for skill in &mut response.skills {
        if (overwrite_existing_source || skill.source_path.is_none())
            && let Some(container_path) = container_skill_paths.get(&skill.path)
        {
            skill.source_path = Some(container_path.clone());
        }
    }
}

fn display_skill_path(path: &Path, container_skill_paths: &ContainerSkillPaths) -> PathBuf {
    container_skill_paths
        .get(path)
        .cloned()
        .unwrap_or_else(|| path.to_path_buf())
}

fn safe_container_skill_segment(value: &str) -> String {
    let segment = value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => ch,
            _ => '_',
        })
        .collect::<String>();
    if segment.is_empty() {
        "skill".to_string()
    } else {
        segment
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{ModelContentItem, SkillMetadata};

    #[test]
    fn skill_user_fragment_wraps_loaded_skill_contents() {
        let path = PathBuf::from("/tmp/demo/SKILL.md");
        let fragment = skill_user_fragment(
            &SkillInjections {
                items: vec![mai_skills::LoadedSkill {
                    metadata: SkillMetadata {
                        name: "demo".to_string(),
                        description: "Demo skill".to_string(),
                        short_description: None,
                        path: path.clone(),
                        source_path: None,
                        scope: SkillScope::Repo,
                        enabled: true,
                        interface: None,
                        dependencies: None,
                        policy: None,
                    },
                    contents: "skill body".to_string(),
                }],
                warnings: Vec::new(),
            },
            &ContainerSkillPaths::default(),
        )
        .expect("fragment");
        assert!(matches!(
            fragment,
            ModelInputItem::Message { role, content }
                if role == "user"
                    && matches!(&content[0], ModelContentItem::InputText { text }
                        if text.contains("<skill>")
                            && text.contains("<name>demo</name>")
                            && text.contains(path.to_string_lossy().as_ref())
                            && text.contains("skill body"))
        ));
    }

    #[test]
    fn skill_user_fragment_uses_container_skill_path_when_synced() {
        let path = PathBuf::from("/tmp/system/demo/SKILL.md");
        let container_path = PathBuf::from("/tmp/.mai-team/skills/system/demo/SKILL.md");
        let mut paths = HashMap::new();
        paths.insert(path.clone(), container_path.clone());
        let fragment = skill_user_fragment(
            &SkillInjections {
                items: vec![mai_skills::LoadedSkill {
                    metadata: SkillMetadata {
                        name: "demo".to_string(),
                        description: "Demo skill".to_string(),
                        short_description: None,
                        path: path.clone(),
                        source_path: None,
                        scope: SkillScope::System,
                        enabled: true,
                        interface: None,
                        dependencies: None,
                        policy: None,
                    },
                    contents: "skill body".to_string(),
                }],
                warnings: Vec::new(),
            },
            &ContainerSkillPaths::from_paths(paths),
        )
        .expect("fragment");

        assert!(matches!(
            fragment,
            ModelInputItem::Message { role, content }
                if role == "user"
                    && matches!(&content[0], ModelContentItem::InputText { text }
                        if text.contains(container_path.to_string_lossy().as_ref())
                            && !text.contains(path.to_string_lossy().as_ref()))
        ));
    }

    #[test]
    fn container_skill_dir_uses_temp_root_outside_workspace() {
        let skill = SkillMetadata {
            name: "demo".to_string(),
            description: "Demo skill".to_string(),
            short_description: None,
            path: PathBuf::from("/tmp/system/demo/SKILL.md"),
            source_path: None,
            scope: SkillScope::System,
            enabled: true,
            interface: None,
            dependencies: None,
            policy: None,
        };

        assert_eq!(
            container_skill_dir(&skill),
            PathBuf::from("/tmp/.mai-team/skills/system/demo")
        );
    }
}
