use crate::names::{
    TOOL_REQUEST_USER_INPUT, TOOL_SAVE_ARTIFACT, TOOL_SAVE_TASK_PLAN, TOOL_SUBMIT_REVIEW_RESULT,
    TOOL_UPDATE_TODO_LIST,
};
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
            TOOL_UPDATE_TODO_LIST,
            "Update your task todo list. Replaces the entire list each call. \
             Call this with an items array. Each item has a step description and a status \
             (pending, in_progress, or completed). \
             At most one item should be in_progress at a time. \
             Use this to communicate your progress plan to the user.",
            object_schema(vec![(
                "items",
                json!({
                    "type": "array",
                    "description": "The complete list of todo items. Replaces any previous list.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "step": {
                                "type": "string",
                                "description": "Description of the task step."
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Status of this step. At most one item should be in_progress."
                            }
                        },
                        "required": ["step", "status"],
                        "additionalProperties": false
                    }
                }),
                true,
            )]),
        ),
        ToolDefinition::function(
            TOOL_REQUEST_USER_INPUT,
            "Ask the user a structured question with multiple-choice options. \
             Use this during planning to resolve ambiguity, confirm assumptions, or choose between meaningful tradeoffs. \
             Each question must materially change the plan, confirm an assumption, or choose between tradeoffs.",
            object_schema(vec![
                (
                    "header",
                    json!({ "type": "string", "description": "Short section header for the question group." }),
                    true,
                ),
                (
                    "questions",
                    json!({
                        "type": "array",
                        "description": "List of questions to ask the user.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string", "description": "Unique identifier for this question." },
                                "question": { "type": "string", "description": "The question text." },
                                "options": {
                                    "type": "array",
                                    "description": "Available choices. 2-4 options, each with label and description.",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": { "type": "string", "description": "Short option label." },
                                            "description": { "type": "string", "description": "Explanation of what this option means." }
                                        },
                                        "required": ["label", "description"],
                                        "additionalProperties": false
                                    },
                                    "minItems": 2,
                                    "maxItems": 4
                                }
                            },
                            "required": ["id", "question", "options"],
                            "additionalProperties": false
                        }
                    }),
                    true,
                ),
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
