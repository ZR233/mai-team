use mai_mcp::McpTool;
use mai_protocol::ToolDefinition;

mod definitions;
mod names;
mod routing;
mod schema;

pub use names::*;
pub use routing::{RoutedTool, route_tool};

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
    fn routes_shared_names_without_exposing_legacy_aliases() {
        assert_eq!(
            route_tool("container.exec"),
            RoutedTool::Unknown("container.exec".to_string())
        );
        assert_eq!(route_tool("container_exec"), RoutedTool::ContainerExec);
        assert_eq!(route_tool("container_copy"), RoutedTool::ContainerCopy);
        assert_eq!(route_tool("apply_patch"), RoutedTool::ApplyPatch);
        assert_eq!(route_tool("search_files"), RoutedTool::SearchFiles);
        assert_eq!(
            route_tool("github_api_get"),
            RoutedTool::Unknown("github_api_get".to_string())
        );
        assert_eq!(
            route_tool("git_worktree_info"),
            RoutedTool::Unknown("git_worktree_info".to_string())
        );
        assert_eq!(
            route_tool("container_cp_upload"),
            RoutedTool::Unknown("container_cp_upload".to_string())
        );
    }

    #[test]
    fn builtin_definitions_are_product_tools_only() {
        let tools = build_tool_definitions(&[]);
        let names = tool_names(&tools);

        assert!(names.contains(&TOOL_GITHUB_API_REQUEST));
        assert!(names.contains(&TOOL_SAVE_ARTIFACT));
        assert!(tools.iter().all(|tool| tool.kind == "function"));
        for legacy in [
            TOOL_CONTAINER_EXEC,
            TOOL_READ_FILE,
            TOOL_LIST_FILES,
            TOOL_SEARCH_FILES,
            TOOL_APPLY_PATCH,
            TOOL_SPAWN_AGENT,
            TOOL_SEND_INPUT,
            TOOL_WAIT_AGENT,
            TOOL_LIST_AGENTS,
            TOOL_CLOSE_AGENT,
            TOOL_RESUME_AGENT,
            TOOL_GIT_STATUS,
            TOOL_GIT_PUSH,
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
    fn update_todo_list_schema_uses_items_field() {
        let tools = build_tool_definitions(&[]);
        let update_todo = tools
            .iter()
            .find(|tool| tool.name == TOOL_UPDATE_TODO_LIST)
            .expect("update_todo_list tool");
        let properties = update_todo
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert!(properties.contains_key("items"));
        assert!(!properties.contains_key("todos"));
        assert_eq!(
            update_todo
                .parameters
                .pointer("/properties/items/items/properties/status/enum"),
            Some(&json!(["pending", "inProgress", "completed"]))
        );
        assert_eq!(
            update_todo.parameters.get("required"),
            Some(&json!(["items"]))
        );
    }

    #[test]
    fn request_user_input_schema_uses_pl_core_question_shape() {
        let tools = build_tool_definitions(&[]);
        let ask = tools
            .iter()
            .find(|tool| tool.name == TOOL_REQUEST_USER_INPUT)
            .expect("request_user_input");
        let properties = ask
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert!(properties.contains_key("questions"));
        assert!(properties.get("header").is_none());
        assert!(
            ask.parameters
                .pointer("/properties/questions/items/properties/header")
                .is_some()
        );
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
