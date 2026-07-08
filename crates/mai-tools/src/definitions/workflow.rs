use crate::names::{TOOL_SAVE_ARTIFACT, TOOL_SAVE_TASK_PLAN, TOOL_SUBMIT_REVIEW_RESULT};
use crate::schema::object_schema;
use mai_protocol::ToolDefinition;
use serde_json::json;

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            TOOL_SAVE_TASK_PLAN,
            "Save or update the task plan. Each call replaces the previous plan and increments the version. \
             Plans must be decision-complete: the Executor should not need to make design decisions. \
             Use request_user_input to resolve any remaining ambiguity before saving.",
            object_schema(vec![
                ("title", json!({ "type": "string" }), true),
                ("markdown", json!({ "type": "string" }), true),
            ]),
        ),
        ToolDefinition::function(
            TOOL_SUBMIT_REVIEW_RESULT,
            "Submit the structured review result for a task workflow. Only reviewer agents attached to a task may call this.",
            object_schema(vec![
                ("passed", json!({ "type": "boolean" }), true),
                ("findings", json!({ "type": "string" }), true),
                ("summary", json!({ "type": "string" }), true),
            ]),
        ),
        ToolDefinition::function(
            TOOL_SAVE_ARTIFACT,
            "Register a file as a downloadable artifact for the user. \
             Use this when you have produced a deliverable file (report, code output, data export, generated document, etc.) \
             that the user should be able to download from the web interface.",
            object_schema(vec![
                (
                    "path",
                    json!({ "type": "string", "description": "Absolute path of the file inside the container." }),
                    true,
                ),
                (
                    "name",
                    json!({ "type": "string", "description": "Display name for the artifact. Defaults to the filename from path." }),
                    false,
                ),
            ]),
        ),
    ]
}
