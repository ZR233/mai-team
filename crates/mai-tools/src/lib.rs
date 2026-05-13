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
    fn routes_dot_aliases() {
        assert_eq!(route_tool("container.exec"), RoutedTool::ContainerExec);
        assert_eq!(route_tool("container_exec"), RoutedTool::ContainerExec);
    }

    #[test]
    fn routes_definition_only_file_tools() {
        assert_eq!(route_tool("apply_patch"), RoutedTool::ApplyPatch);
        assert_eq!(route_tool("search_files"), RoutedTool::SearchFiles);
    }

    #[test]
    fn builds_builtin_definitions() {
        let tools = build_tool_definitions(&[]);
        assert!(tools.iter().any(|tool| tool.name == TOOL_SPAWN_AGENT));
        assert!(tools.iter().all(|tool| tool.kind == "function"));
    }

    #[test]
    fn definition_only_file_tools_are_not_exposed_by_default() {
        let tools = build_tool_definitions(&[]);
        let names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert!(!names.contains(&TOOL_APPLY_PATCH));
        assert!(!names.contains(&TOOL_SEARCH_FILES));
    }

    #[test]
    fn spawn_agent_schema_does_not_expose_model_selection() {
        let tools = build_tool_definitions(&[]);
        let spawn = tools
            .iter()
            .find(|tool| tool.name == TOOL_SPAWN_AGENT)
            .expect("spawn tool");
        let properties = spawn
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert!(properties.contains_key("name"));
        assert!(properties.contains_key("message"));
        assert!(properties.contains_key("role"));
        assert!(properties.contains_key("agent_type"));
        assert!(properties.contains_key("model"));
        assert!(!properties.contains_key("provider_id"));
    }

    #[test]
    fn collab_items_schema_accepts_skill_items() {
        let tools = build_tool_definitions(&[]);
        for tool_name in [TOOL_SPAWN_AGENT, TOOL_SEND_INPUT] {
            let tool = tools
                .iter()
                .find(|tool| tool.name == tool_name)
                .expect("tool");
            let item_variants = tool
                .parameters
                .pointer("/properties/items/items/oneOf")
                .and_then(Value::as_array)
                .expect("oneOf item variants");
            assert_eq!(item_variants.len(), 2);
            let skill_variant = item_variants
                .iter()
                .find(|variant| {
                    variant
                        .pointer("/properties/type/enum/0")
                        .and_then(Value::as_str)
                        == Some("skill")
                })
                .expect("skill variant");
            assert!(
                skill_variant
                    .get("anyOf")
                    .and_then(Value::as_array)
                    .is_some_and(|items| items.len() == 2)
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
            update_todo.parameters.get("required"),
            Some(&json!(["items"]))
        );
    }

    #[test]
    fn codex_compatible_subagent_tools_are_exposed() {
        let tools = build_tool_definitions(&[]);
        let names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&TOOL_SEND_INPUT));
        assert!(names.contains(&TOOL_WAIT_AGENT));
        assert!(names.contains(&TOOL_CLOSE_AGENT));
        assert!(names.contains(&TOOL_RESUME_AGENT));
        let wait = tools
            .iter()
            .find(|tool| tool.name == TOOL_WAIT_AGENT)
            .expect("wait_agent");
        let properties = wait
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .expect("wait properties");
        assert!(properties.contains_key("targets"));
        assert!(properties.contains_key("timeout_ms"));
        assert!(properties.contains_key("agent_id"));
        assert_eq!(properties["timeout_ms"]["type"].as_str(), Some("integer"));
        assert_eq!(properties["timeout_ms"]["minimum"].as_u64(), Some(100));
        assert!(properties["timeout_ms"].get("maximum").is_none());
        assert_eq!(properties["timeout_secs"]["type"].as_str(), Some("integer"));
        assert_eq!(properties["timeout_secs"]["minimum"].as_u64(), Some(1));
        assert!(properties["timeout_secs"].get("maximum").is_none());
        assert_eq!(route_tool("send_input"), RoutedTool::SendInput);
        assert_eq!(route_tool("resume_agent"), RoutedTool::ResumeAgent);
        assert_eq!(route_tool("github_api_get"), RoutedTool::GithubApiGet);
    }

    #[test]
    fn container_exec_timeout_is_optional_without_budget_cap() {
        let tools = build_tool_definitions(&[]);
        let exec = tools
            .iter()
            .find(|tool| tool.name == TOOL_CONTAINER_EXEC)
            .expect("container_exec");
        let timeout = exec
            .parameters
            .pointer("/properties/timeout_secs")
            .expect("timeout_secs schema");
        assert_eq!(timeout["type"].as_str(), Some("integer"));
        assert_eq!(timeout["minimum"].as_u64(), Some(1));
        assert!(timeout.get("maximum").is_none());
        assert!(
            !exec
                .parameters
                .pointer("/required")
                .and_then(Value::as_array)
                .is_some_and(|required| required.iter().any(|item| item == "timeout_secs"))
        );
    }
}
