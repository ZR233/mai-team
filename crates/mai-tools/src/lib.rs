use mai_mcp::McpTool;
use mai_protocol::ToolDefinition;
use serde_json::{Value, json};

pub const TOOL_CONTAINER_EXEC: &str = "container_exec";
pub const TOOL_CONTAINER_CP_UPLOAD: &str = "container_cp_upload";
pub const TOOL_CONTAINER_CP_DOWNLOAD: &str = "container_cp_download";
pub const TOOL_SPAWN_AGENT: &str = "spawn_agent";
pub const TOOL_SEND_MESSAGE: &str = "send_message";
pub const TOOL_WAIT_AGENT: &str = "wait_agent";
pub const TOOL_LIST_AGENTS: &str = "list_agents";
pub const TOOL_CLOSE_AGENT: &str = "close_agent";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutedTool {
    ContainerExec,
    ContainerCpUpload,
    ContainerCpDownload,
    SpawnAgent,
    SendMessage,
    WaitAgent,
    ListAgents,
    CloseAgent,
    Mcp(String),
    Unknown(String),
}

pub fn route_tool(name: &str) -> RoutedTool {
    match normalize_name(name).as_str() {
        TOOL_CONTAINER_EXEC => RoutedTool::ContainerExec,
        TOOL_CONTAINER_CP_UPLOAD => RoutedTool::ContainerCpUpload,
        TOOL_CONTAINER_CP_DOWNLOAD => RoutedTool::ContainerCpDownload,
        TOOL_SPAWN_AGENT => RoutedTool::SpawnAgent,
        TOOL_SEND_MESSAGE => RoutedTool::SendMessage,
        TOOL_WAIT_AGENT => RoutedTool::WaitAgent,
        TOOL_LIST_AGENTS => RoutedTool::ListAgents,
        TOOL_CLOSE_AGENT => RoutedTool::CloseAgent,
        normalized if normalized.starts_with("mcp__") => RoutedTool::Mcp(normalized.to_string()),
        normalized => RoutedTool::Unknown(normalized.to_string()),
    }
}

pub fn build_tool_definitions(mcp_tools: &[McpTool]) -> Vec<ToolDefinition> {
    let mut tools = builtin_tool_definitions();
    tools.extend(mcp_tools.iter().map(|tool| {
        ToolDefinition::function(
            tool.model_name.clone(),
            if tool.description.is_empty() {
                format!("Call MCP tool `{}` on server `{}`.", tool.name, tool.server)
            } else {
                tool.description.clone()
            },
            tool.input_schema.clone(),
        )
    }));
    tools
}

fn builtin_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            TOOL_CONTAINER_EXEC,
            "Execute a shell command inside this agent's Docker container.",
            object_schema(vec![
                ("command", json!({ "type": "string" }), true),
                ("cwd", json!({ "type": "string" }), false),
                (
                    "timeout_secs",
                    json!({ "type": "integer", "minimum": 1, "maximum": 600 }),
                    false,
                ),
            ]),
        ),
        ToolDefinition::function(
            TOOL_CONTAINER_CP_UPLOAD,
            "Write a base64 encoded file into this agent's Docker container.",
            object_schema(vec![
                ("path", json!({ "type": "string" }), true),
                ("content_base64", json!({ "type": "string" }), true),
            ]),
        ),
        ToolDefinition::function(
            TOOL_CONTAINER_CP_DOWNLOAD,
            "Export a file or directory from this agent's Docker container as a base64 encoded tar stream.",
            object_schema(vec![("path", json!({ "type": "string" }), true)]),
        ),
        ToolDefinition::function(
            TOOL_SPAWN_AGENT,
            "Create a child agent with its own Docker container. Optionally send it an initial task.",
            object_schema(vec![
                ("name", json!({ "type": "string" }), false),
                ("message", json!({ "type": "string" }), false),
                ("provider_id", json!({ "type": "string" }), false),
                ("model", json!({ "type": "string" }), false),
            ]),
        ),
        ToolDefinition::function(
            TOOL_SEND_MESSAGE,
            "Send a task message to an existing agent.",
            object_schema(vec![
                ("agent_id", json!({ "type": "string" }), true),
                ("session_id", json!({ "type": "string" }), false),
                ("message", json!({ "type": "string" }), true),
            ]),
        ),
        ToolDefinition::function(
            TOOL_WAIT_AGENT,
            "Wait for an agent to finish its current turn.",
            object_schema(vec![
                ("agent_id", json!({ "type": "string" }), true),
                (
                    "timeout_secs",
                    json!({ "type": "integer", "minimum": 1, "maximum": 3600 }),
                    false,
                ),
            ]),
        ),
        ToolDefinition::function(
            TOOL_LIST_AGENTS,
            "List live agents, their statuses, containers, and recent task summaries.",
            object_schema(Vec::new()),
        ),
        ToolDefinition::function(
            TOOL_CLOSE_AGENT,
            "Stop and remove an agent's Docker container.",
            object_schema(vec![("agent_id", json!({ "type": "string" }), true)]),
        ),
    ]
}

fn object_schema(fields: Vec<(&str, Value, bool)>) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, schema, is_required) in fields {
        properties.insert(name.to_string(), schema);
        if is_required {
            required.push(Value::String(name.to_string()));
        }
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn normalize_name(name: &str) -> String {
    name.replace('.', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_dot_aliases() {
        assert_eq!(route_tool("container.exec"), RoutedTool::ContainerExec);
        assert_eq!(route_tool("container_exec"), RoutedTool::ContainerExec);
    }

    #[test]
    fn builds_builtin_definitions() {
        let tools = build_tool_definitions(&[]);
        assert!(tools.iter().any(|tool| tool.name == TOOL_SPAWN_AGENT));
        assert!(tools.iter().all(|tool| tool.kind == "function"));
    }
}
