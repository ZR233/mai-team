use super::super::names::{
    TOOL_READ_TOOL_ARTIFACT, TOOL_SAVE_ARTIFACT, TOOL_SAVE_TASK_PLAN, TOOL_SUBMIT_REVIEW_RESULT,
};
use pl_core::{ToolInputSchemaField, function_tool_schema};
use pl_model::ToolSchema;
use serde_json::json;

pub(crate) fn definitions() -> Vec<ToolSchema> {
    vec![
        function_tool_schema(
            TOOL_SAVE_TASK_PLAN,
            "Save or update the task plan. Each call replaces the previous plan and increments the version. \
             Plans must be decision-complete: the Executor should not need to make design decisions. \
             Use request_user_input to resolve any remaining ambiguity before saving.",
            [
                ToolInputSchemaField::required("title", json!({ "type": "string" })),
                ToolInputSchemaField::required("markdown", json!({ "type": "string" })),
            ],
        ),
        function_tool_schema(
            TOOL_SUBMIT_REVIEW_RESULT,
            "Submit the structured review result for a task workflow. Only reviewer agents attached to a task may call this.",
            [
                ToolInputSchemaField::required("passed", json!({ "type": "boolean" })),
                ToolInputSchemaField::required("findings", json!({ "type": "string" })),
                ToolInputSchemaField::required("summary", json!({ "type": "string" })),
            ],
        ),
        function_tool_schema(
            TOOL_SAVE_ARTIFACT,
            "Register a file as a downloadable artifact for the user. \
             Use this when you have produced a deliverable file (report, code output, data export, generated document, etc.) \
             that the user should be able to download from the web interface.",
            [
                ToolInputSchemaField::required(
                    "path",
                    json!({ "type": "string", "description": "Absolute path of the file inside the agent workspace." }),
                ),
                ToolInputSchemaField::optional(
                    "name",
                    json!({ "type": "string", "description": "Display name for the artifact. Defaults to the filename from path." }),
                ),
            ],
        ),
        function_tool_schema(
            TOOL_READ_TOOL_ARTIFACT,
            "Read one bounded range from a full output artifact returned by exec or write_stdin. Copy outputArtifacts[].call_id to callId and outputArtifacts[].id to artifactId exactly; use the receipt values, not outputFile, a tool item ID, or a provider call ID. Select exactly one range kind: lines are UTF-8 text with a 1-based offset and a limit of at most 500; bytes are base64 with a 0-based offset and a limit of at most 65536.",
            [
                ToolInputSchemaField::required("callId", json!({ "type": "string" })),
                ToolInputSchemaField::required("artifactId", json!({ "type": "string" })),
                ToolInputSchemaField::required(
                    "range",
                    json!({
                        "type": "string",
                        "enum": ["lines", "bytes"],
                        "description": "Use lines for text output and bytes for binary output."
                    }),
                ),
                ToolInputSchemaField::optional(
                    "offset",
                    json!({
                        "type": "integer",
                        "minimum": 0,
                        "description": "1-based first line for range=lines; 0-based first byte for range=bytes. Defaults to 1 or 0 respectively."
                    }),
                ),
                ToolInputSchemaField::optional(
                    "limit",
                    json!({
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 65536,
                        "description": "Maximum lines (at most 500) or bytes (at most 65536) to return."
                    }),
                ),
            ],
        ),
    ]
}
