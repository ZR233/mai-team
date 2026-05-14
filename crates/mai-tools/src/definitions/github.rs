use crate::names::TOOL_GITHUB_API_GET;
use crate::schema::object_schema;
use mai_protocol::ToolDefinition;
use serde_json::json;

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition::function(
        TOOL_GITHUB_API_GET,
        "Call the current Mai project's GitHub REST API with a GET request. \
         Use this only as a read-only fallback when GitHub MCP tools do not expose a needed PR endpoint. \
         The path must be a GitHub API path such as `/repos/OWNER/REPO/pulls/123/reviews`; credentials are supplied server-side.",
        object_schema(vec![(
            "path",
            json!({
                "type": "string",
                "description": "GitHub API path beginning with `/`, optionally including a query string."
            }),
            true,
        )]),
    )]
}
