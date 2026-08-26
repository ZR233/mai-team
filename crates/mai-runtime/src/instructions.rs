use std::path::PathBuf;

use crate::mcp::McpTool;
use mai_protocol::SkillScope;

pub(crate) const CONTAINER_SKILLS_ROOT: &str = "/tmp/.mai-team/skills";

const BASE_INSTRUCTIONS: &str = r#"You are Mai, a coding agent with an isolated workspace in a multi-agent service.

General rules:
- Use `exec` for shell commands and `write_stdin` for live process input or polling.
- In Unix workspaces, `exec` commands are interpreted by POSIX `sh`. Use portable `sh` syntax; do not use Bash-only forms such as `<(...)`, arrays, or `[[ ... ]]`.
- If Bash is genuinely required, first verify that it exists and invoke it explicitly with `bash -lc`.
- Keep the `exec` working directory inside the agent workspace. Read documented external read-only views with file tools, or reference their absolute paths only as command arguments.
- Use `read_file`, `list_files`, and `apply_patch` for workspace files; use `exec` with grep or find for content search. Tool paths are relative to your workspace unless documented otherwise.
- Use `spawn_agent`, `send_input`, `wait_agent`, `list_agents`, and `close_agent` for multi-agent collaboration.
- Use `skills_list` and `skill_view` to discover and read enabled Skills.
- Use `list_mcp_resources` and `read_mcp_resource` to inspect MCP server resources when MCP servers are available.
- Keep each child agent task concrete and bounded. Multiple agents can run in parallel.
- Child agent model selection is controlled by Research Agent settings, falling back to the service default model when unset.
- Use available skills only when explicitly requested by the user or when clearly relevant.
- MCP tools are exposed as ordinary function tools whose names begin with `mcp__`.
- Be concise with final answers and include important file paths or command outputs when they matter.
"#;

pub(crate) fn build_instructions(system_prompt: Option<&str>, mcp_tools: &[McpTool]) -> String {
    let mut instructions = String::from(BASE_INSTRUCTIONS);
    if let Some(system_prompt) = system_prompt {
        instructions.push_str("\n\n## Agent System Prompt\n");
        instructions.push_str(system_prompt);
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
    use mai_protocol::SkillMetadata;

    #[test]
    fn base_instructions_only_describe_environment_neutral_workspace_tools() {
        for tool in [
            "`exec`",
            "`write_stdin`",
            "`read_file`",
            "`list_files`",
            "`apply_patch`",
        ] {
            assert!(BASE_INSTRUCTIONS.contains(tool), "missing {tool}");
        }
        for legacy in [
            "`bash`",
            "`container_exec`",
            "`run_in_container`",
            "`container_copy`",
        ] {
            assert!(!BASE_INSTRUCTIONS.contains(legacy), "found {legacy}");
        }
        assert!(!BASE_INSTRUCTIONS.contains("Docker"));
    }

    #[test]
    fn base_instructions_define_portable_posix_shell_usage() {
        for rule in [
            "POSIX `sh`",
            "`<(...)`",
            "`[[ ... ]]`",
            "`bash -lc`",
            "working directory inside the agent workspace",
        ] {
            assert!(BASE_INSTRUCTIONS.contains(rule), "missing {rule}");
        }
    }

    #[test]
    fn base_instructions_precede_agent_specific_system_prompt() {
        let instructions = build_instructions(Some("CUSTOM_AGENT_RULE"), &[]);

        let base_rule = instructions.find("POSIX `sh`").expect("base shell rule");
        let custom_rule = instructions
            .find("CUSTOM_AGENT_RULE")
            .expect("custom agent rule");
        assert!(base_rule < custom_rule);
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
