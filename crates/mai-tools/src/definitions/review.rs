use crate::names::TOOL_QUEUE_PROJECT_REVIEW_PRS;
use crate::schema::object_schema;
use mai_protocol::ToolDefinition;
use serde_json::json;

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition::function(
        TOOL_QUEUE_PROJECT_REVIEW_PRS,
        "Queue one or more pull requests for the current Mai project's automatic review pool. \
         The server infers the project from the calling agent; do not provide a project id. \
         Use this only from project PR selector or reviewer workflows.",
        object_schema(vec![(
            "prs",
            json!({
                "type": "array",
                "description": "Pull requests to queue for review.",
                "items": {
                    "type": "object",
                    "properties": {
                        "number": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "GitHub pull request number."
                        },
                        "head_sha": {
                            "type": "string",
                            "description": "Optional current PR head commit SHA."
                        },
                        "reason": {
                            "type": "string",
                            "description": "Optional short reason this PR was selected."
                        }
                    },
                    "required": ["number"],
                    "additionalProperties": false
                }
            }),
            true,
        )]),
    )]
}
