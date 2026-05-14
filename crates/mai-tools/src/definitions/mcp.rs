use crate::names::{
    TOOL_LIST_MCP_RESOURCE_TEMPLATES, TOOL_LIST_MCP_RESOURCES, TOOL_READ_MCP_RESOURCE,
};
use crate::schema::object_schema;
use mai_protocol::ToolDefinition;
use serde_json::json;

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            TOOL_LIST_MCP_RESOURCES,
            "Lists resources provided by MCP servers.",
            object_schema(vec![
                ("server", json!({ "type": "string" }), false),
                ("cursor", json!({ "type": "string" }), false),
            ]),
        ),
        ToolDefinition::function(
            TOOL_LIST_MCP_RESOURCE_TEMPLATES,
            "Lists resource templates provided by MCP servers.",
            object_schema(vec![
                ("server", json!({ "type": "string" }), false),
                ("cursor", json!({ "type": "string" }), false),
            ]),
        ),
        ToolDefinition::function(
            TOOL_READ_MCP_RESOURCE,
            "Read a specific resource from an MCP server.",
            object_schema(vec![
                ("server", json!({ "type": "string" }), true),
                ("uri", json!({ "type": "string" }), true),
            ]),
        ),
    ]
}
