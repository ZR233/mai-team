use crate::names::{
    TOOL_GIT_BRANCH, TOOL_GIT_COMMIT, TOOL_GIT_DIFF, TOOL_GIT_FETCH, TOOL_GIT_PUSH,
    TOOL_GIT_STATUS, TOOL_GIT_SYNC_DEFAULT_BRANCH, TOOL_GIT_WORKTREE_INFO,
};
use crate::schema::object_schema;
use mai_protocol::ToolDefinition;
use serde_json::json;

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            TOOL_GIT_STATUS,
            "Show git working tree status for this project agent worktree.",
            object_schema(vec![]),
        ),
        ToolDefinition::function(
            TOOL_GIT_DIFF,
            "Show git diff for this project agent worktree.",
            object_schema(vec![
                ("staged", json!({ "type": "boolean" }), false),
                ("path", json!({ "type": "string" }), false),
            ]),
        ),
        ToolDefinition::function(
            TOOL_GIT_BRANCH,
            "List branches or create/switch the current branch in this project agent worktree.",
            object_schema(vec![
                (
                    "action",
                    json!({
                        "type": "string",
                        "enum": ["list", "switch", "create"]
                    }),
                    false,
                ),
                ("name", json!({ "type": "string" }), false),
                ("start_point", json!({ "type": "string" }), false),
            ]),
        ),
        ToolDefinition::function(
            TOOL_GIT_FETCH,
            "Fetch from the project repository remote using host-injected project credentials.",
            object_schema(vec![
                ("remote", json!({ "type": "string" }), false),
                ("refspec", json!({ "type": "string" }), false),
                ("prune", json!({ "type": "boolean" }), false),
            ]),
        ),
        ToolDefinition::function(
            TOOL_GIT_COMMIT,
            "Create a git commit in this project agent worktree.",
            object_schema(vec![
                ("message", json!({ "type": "string" }), true),
                ("all", json!({ "type": "boolean" }), false),
            ]),
        ),
        ToolDefinition::function(
            TOOL_GIT_PUSH,
            "Push the current branch using host-injected project credentials.",
            object_schema(vec![
                ("remote", json!({ "type": "string" }), false),
                ("branch", json!({ "type": "string" }), false),
                ("set_upstream", json!({ "type": "boolean" }), false),
            ]),
        ),
        ToolDefinition::function(
            TOOL_GIT_WORKTREE_INFO,
            "Show information about this project agent git worktree.",
            object_schema(vec![]),
        ),
        ToolDefinition::function(
            TOOL_GIT_SYNC_DEFAULT_BRANCH,
            "Sync this project agent worktree with the project's latest default branch.",
            object_schema(vec![]),
        ),
    ]
}
