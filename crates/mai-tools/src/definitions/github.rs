use crate::names::TOOL_GITHUB_API_REQUEST;
use pl_core::{ToolInputSchemaField, function_tool_schema};
use pl_model::ToolSchema;
use serde_json::json;

pub(crate) fn definitions() -> Vec<ToolSchema> {
    vec![function_tool_schema(
        TOOL_GITHUB_API_REQUEST,
        "Call the current Mai project's GitHub REST API through the managed gh sidecar. \
             Use this for PR review submission, issue comments, labels, and other GitHub reads or writes. \
             For pull request reviews, submit the final review in one single POST to `/repos/OWNER/REPO/pulls/PR/reviews` with `event`, non-empty `body`, and optional inline comments in the `comments` array; do not create pending reviews, submit `/reviews/ID/events`, or POST inline comments to `/pulls/PR/comments`. \
             Credentials are supplied server-side and are not available to the agent container.",
        [
            ToolInputSchemaField::required(
                "method",
                json!({
                    "type": "string",
                    "enum": ["GET", "POST", "PATCH", "PUT", "DELETE"],
                    "description": "HTTP method for gh api."
                }),
            ),
            ToolInputSchemaField::required(
                "path",
                json!({
                    "type": "string",
                    "description": "GitHub API path beginning with `/`, optionally including a query string."
                }),
            ),
            ToolInputSchemaField::optional(
                "body",
                json!({
                    "type": "object",
                    "description": "Optional JSON object request body passed to gh api via stdin. Do not provide this field as a JSON-encoded string."
                }),
            ),
        ],
    )]
}
