use crate::names::{TOOL_APPLY_PATCH, TOOL_LIST_FILES, TOOL_READ_FILE, TOOL_SEARCH_FILES};
use crate::schema::object_schema;
use mai_protocol::ToolDefinition;
use serde_json::json;

const APPLY_PATCH_DESCRIPTION: &str = r#"Use the `apply_patch` tool to edit files.

The patch format is a stripped-down, file-oriented diff envelope:

*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
@@
-print("Hi")
+print("Hello, world!")
*** Delete File: obsolete.txt
*** End Patch

Each operation starts with one of:
- `*** Add File: <path>`
- `*** Delete File: <path>`
- `*** Update File: <path>`

Update sections contain hunks introduced by `@@`; hunk lines start with a space, `-`, or `+`. File references must be relative, never absolute."#;

pub(crate) fn exposed_definitions() -> Vec<ToolDefinition> {
    vec![read_file_definition(), list_files_definition()]
}

#[allow(dead_code)]
pub(crate) fn definition_only_definitions() -> Vec<ToolDefinition> {
    vec![apply_patch_definition(), search_files_definition()]
}

fn read_file_definition() -> ToolDefinition {
    ToolDefinition::function(
        TOOL_READ_FILE,
        "Read a text file inside this agent's Docker container with bounded output. Use line_start/line_count for source files or offset/max_bytes for byte paging.",
        object_schema(vec![
            ("path", json!({ "type": "string" }), true),
            ("cwd", json!({ "type": "string" }), false),
            (
                "line_start",
                json!({ "type": "integer", "minimum": 1 }),
                false,
            ),
            (
                "line_count",
                json!({ "type": "integer", "minimum": 1 }),
                false,
            ),
            ("offset", json!({ "type": "integer", "minimum": 0 }), false),
            (
                "max_bytes",
                json!({ "type": "integer", "minimum": 1 }),
                false,
            ),
        ]),
    )
}

fn list_files_definition() -> ToolDefinition {
    ToolDefinition::function(
        TOOL_LIST_FILES,
        "List files inside this agent's Docker container with bounded output.",
        object_schema(vec![
            ("path", json!({ "type": "string" }), false),
            ("cwd", json!({ "type": "string" }), false),
            ("pattern", json!({ "type": "string" }), false),
            (
                "max_files",
                json!({ "type": "integer", "minimum": 1 }),
                false,
            ),
        ]),
    )
}

fn apply_patch_definition() -> ToolDefinition {
    ToolDefinition::function(
        TOOL_APPLY_PATCH,
        APPLY_PATCH_DESCRIPTION,
        object_schema(vec![(
            "input",
            json!({
                "type": "string",
                "description": "The entire contents of the apply_patch command."
            }),
            true,
        )]),
    )
}

fn search_files_definition() -> ToolDefinition {
    ToolDefinition::function(
        TOOL_SEARCH_FILES,
        "Search file contents inside this agent's Docker container. Intended for ripgrep-style content search with optional path and glob filters.",
        object_schema(vec![
            (
                "query",
                json!({
                    "type": "string",
                    "description": "Text or pattern to search for."
                }),
                true,
            ),
            (
                "path",
                json!({
                    "type": "string",
                    "description": "Directory or file path to search. Defaults to the current working directory."
                }),
                false,
            ),
            ("cwd", json!({ "type": "string" }), false),
            (
                "glob",
                json!({
                    "type": "string",
                    "description": "Optional file glob filter, such as `*.rs`."
                }),
                false,
            ),
            (
                "case_sensitive",
                json!({
                    "type": "boolean",
                    "description": "Whether matching should be case-sensitive."
                }),
                false,
            ),
            (
                "max_matches",
                json!({ "type": "integer", "minimum": 1 }),
                false,
            ),
        ]),
    )
}
