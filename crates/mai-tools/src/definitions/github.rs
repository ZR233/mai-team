use crate::names::{TOOL_GITHUB_API_GET, TOOL_GITHUB_API_REQUEST};
use crate::schema::object_schema;
use mai_protocol::ToolDefinition;
use serde_json::json;

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            TOOL_GITHUB_API_GET,
            "Call the current Mai project's GitHub REST API with a GET request through the managed gh sidecar. \
             The path must be a GitHub API path such as `/repos/OWNER/REPO/pulls/123/reviews`; credentials are supplied server-side.",
            object_schema(vec![(
                "path",
                json!({
                    "type": "string",
                    "description": "GitHub API path beginning with `/`, optionally including a query string."
                }),
                true,
            )]),
        ),
        ToolDefinition::function(
            TOOL_GITHUB_API_REQUEST,
            "Call the current Mai project's GitHub REST API through the managed gh sidecar. \
             Use this for PR review submission, comments, labels, and other GitHub reads or writes. \
             Credentials are supplied server-side and are not available to the agent container.",
            object_schema(vec![
                (
                    "method",
                    json!({
                        "type": "string",
                        "enum": ["GET", "POST", "PATCH", "PUT", "DELETE"],
                        "description": "HTTP method for gh api."
                    }),
                    true,
                ),
                (
                    "path",
                    json!({
                        "type": "string",
                        "description": "GitHub API path beginning with `/`, optionally including a query string."
                    }),
                    true,
                ),
                (
                    "body",
                    json!({
                        "description": "Optional JSON request body passed to gh api via stdin."
                    }),
                    false,
                ),
            ]),
        ),
    ]
}
