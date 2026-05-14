use crate::names::{TOOL_CONTAINER_CP_DOWNLOAD, TOOL_CONTAINER_CP_UPLOAD, TOOL_CONTAINER_EXEC};
use crate::schema::object_schema;
use mai_protocol::ToolDefinition;
use serde_json::json;

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            TOOL_CONTAINER_EXEC,
            "Execute a shell command inside this agent's Docker container. timeout_secs is optional; omit it for no command time limit.",
            object_schema(vec![
                ("command", json!({ "type": "string" }), true),
                ("cwd", json!({ "type": "string" }), false),
                (
                    "timeout_secs",
                    json!({ "type": "integer", "minimum": 1 }),
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
    ]
}
