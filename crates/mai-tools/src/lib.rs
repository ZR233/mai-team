use mai_mcp::McpTool;
use mai_protocol::ToolDefinition;

mod definitions;
mod names;
mod schema;

pub use names::*;

pub fn build_tool_definitions(mcp_tools: &[McpTool]) -> Vec<ToolDefinition> {
    build_tool_definitions_with_filter(mcp_tools, |_| true)
}

pub fn build_tool_definitions_with_filter(
    mcp_tools: &[McpTool],
    allow_tool: impl Fn(&str) -> bool,
) -> Vec<ToolDefinition> {
    let mut tools = definitions::builtin_tool_definitions()
        .into_iter()
        .filter(|tool| allow_tool(&tool.name))
        .collect::<Vec<_>>();
    tools.extend(
        mcp_tools
            .iter()
            .filter(|tool| allow_tool(&tool.model_name))
            .map(|tool| {
                ToolDefinition::function(
                    tool.model_name.clone(),
                    if tool.description.is_empty() {
                        format!("Call MCP tool `{}` on server `{}`.", tool.name, tool.server)
                    } else {
                        tool.description.clone()
                    },
                    tool.input_schema.clone(),
                )
            }),
    );
    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn product_tool_api_is_definition_only() {
        let api = include_str!("lib.rs");

        assert!(!api.contains(&format!("{}{}", "route", "_tool")));
        assert!(!api.contains(&format!("{}{}", "Routed", "Tool")));
    }

    #[test]
    fn builtin_definitions_are_product_tools_only() {
        let tools = build_tool_definitions(&[]);
        let names = tool_names(&tools);

        assert!(names.contains(&TOOL_GITHUB_API_REQUEST));
        assert!(names.contains(&TOOL_SAVE_ARTIFACT));
        assert!(tools.iter().all(|tool| tool.kind == "function"));
        for legacy in [
            pl_core::TOOL_CONTAINER_EXEC,
            pl_core::WorkspaceFileToolKind::ReadFile.name(),
            pl_core::WorkspaceFileToolKind::ListFiles.name(),
            pl_core::WorkspaceFileToolKind::SearchFiles.name(),
            pl_core::WorkspaceFileToolKind::ApplyPatch.name(),
            pl_core::TOOL_SPAWN_AGENT,
            pl_core::TOOL_SEND_INPUT,
            pl_core::TOOL_WAIT_AGENT,
            pl_core::TOOL_LIST_AGENTS,
            pl_core::TOOL_CLOSE_AGENT,
            pl_core::TOOL_RESUME_AGENT,
            "update_todo_list",
            "request_user_input",
            pl_core::TOOL_GIT_STATUS,
            pl_core::TOOL_GIT_PUSH,
            pl_core::TOOL_LIST_MCP_RESOURCES,
            pl_core::TOOL_LIST_MCP_RESOURCE_TEMPLATES,
            pl_core::TOOL_READ_MCP_RESOURCE,
            "github_api_get",
            "send_message",
            "git_worktree_info",
            "container_cp_upload",
            "container_cp_download",
        ] {
            assert!(
                !names.contains(&legacy),
                "{legacy} must be supplied by pl-core or removed"
            );
        }
    }

    #[test]
    fn github_request_schema_covers_read_write_without_credentials() {
        let tools = build_tool_definitions(&[]);
        let request = tools
            .iter()
            .find(|tool| tool.name == TOOL_GITHUB_API_REQUEST)
            .expect("github_api_request");
        assert_eq!(
            request.parameters.get("required"),
            Some(&json!(["method", "path"]))
        );
        let properties = request
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert_eq!(
            properties.get("body").and_then(|schema| schema.get("type")),
            Some(&json!("object"))
        );
        let description = request.description.as_str();
        assert!(description.contains("single POST"));
        assert!(description.contains("event"));
        assert!(description.contains("pending review"));
        for forbidden in ["token", "env", "cwd", "repo_path", "worktree_path"] {
            assert!(
                !properties.contains_key(forbidden),
                "github_api_request exposes forbidden field {forbidden}"
            );
        }
    }

    #[test]
    fn filters_product_and_mcp_tools() {
        let mcp = McpTool {
            server: "s".to_string(),
            name: "n".to_string(),
            model_name: "mcp__s__n".to_string(),
            description: String::new(),
            input_schema: json!({"type":"object"}),
            output_schema: None,
        };
        let tools = build_tool_definitions_with_filter(&[mcp], |name| {
            name == TOOL_SAVE_ARTIFACT || name == "mcp__s__n"
        });
        assert_eq!(tool_names(&tools), vec![TOOL_SAVE_ARTIFACT, "mcp__s__n"]);
    }

    fn tool_names(tools: &[ToolDefinition]) -> Vec<&str> {
        tools.iter().map(|tool| tool.name.as_str()).collect()
    }
}
