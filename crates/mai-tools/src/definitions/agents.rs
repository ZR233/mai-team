use crate::definitions::collab::collab_items_schema;
use crate::names::{
    TOOL_CLOSE_AGENT, TOOL_LIST_AGENTS, TOOL_RESUME_AGENT, TOOL_SEND_INPUT, TOOL_SEND_MESSAGE,
    TOOL_SPAWN_AGENT, TOOL_WAIT_AGENT,
};
use crate::schema::object_schema;
use mai_protocol::ToolDefinition;
use serde_json::json;

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            TOOL_SPAWN_AGENT,
            "Create a child agent with its own Docker container. Spawned agents inherit the parent model by default unless role, model, or reasoning_effort is provided.",
            object_schema(vec![
                (
                    "agent_type",
                    json!({
                        "type": "string",
                        "enum": ["default", "explorer", "worker"],
                        "description": "Codex-compatible agent type. default and worker map to executor; explorer maps to explorer."
                    }),
                    false,
                ),
                (
                    "role",
                    json!({
                        "type": "string",
                        "enum": ["planner", "explorer", "executor", "reviewer"],
                        "description": "Legacy role profile. When set, role model preferences are used."
                    }),
                    false,
                ),
                ("name", json!({ "type": "string" }), false),
                ("message", json!({ "type": "string" }), false),
                ("items", collab_items_schema(), false),
                ("fork_context", json!({ "type": "boolean" }), false),
                ("model", json!({ "type": "string" }), false),
                ("reasoning_effort", json!({ "type": "string" }), false),
            ]),
        ),
        ToolDefinition::function(
            TOOL_SEND_INPUT,
            "Send a message to an existing agent. Use interrupt=true to redirect work immediately; otherwise busy agents queue the input for the next turn.",
            object_schema(vec![
                ("target", json!({ "type": "string" }), true),
                ("message", json!({ "type": "string" }), false),
                ("items", collab_items_schema(), false),
                ("interrupt", json!({ "type": "boolean" }), false),
            ]),
        ),
        ToolDefinition::function(
            TOOL_SEND_MESSAGE,
            "Legacy alias for send_input. Send a task message to an existing agent.",
            object_schema(vec![
                ("agent_id", json!({ "type": "string" }), true),
                ("session_id", json!({ "type": "string" }), false),
                ("message", json!({ "type": "string" }), true),
            ]),
        ),
        ToolDefinition::function(
            TOOL_WAIT_AGENT,
            "Sample one or more agents for a short observation window. Timeout is a normal pending result with diagnostics, not task failure.",
            object_schema(vec![
                (
                    "targets",
                    json!({ "type": "array", "items": { "type": "string" } }),
                    false,
                ),
                ("agent_id", json!({ "type": "string" }), false),
                (
                    "timeout_ms",
                    json!({ "type": "integer", "minimum": 100 }),
                    false,
                ),
                (
                    "timeout_secs",
                    json!({ "type": "integer", "minimum": 1 }),
                    false,
                ),
            ]),
        ),
        ToolDefinition::function(
            TOOL_LIST_AGENTS,
            "List live agents, their statuses, containers, and recent task summaries.",
            object_schema(Vec::new()),
        ),
        ToolDefinition::function(
            TOOL_CLOSE_AGENT,
            "Stop and remove an agent's Docker container while keeping the agent record resumable.",
            object_schema(vec![
                ("target", json!({ "type": "string" }), false),
                ("agent_id", json!({ "type": "string" }), false),
            ]),
        ),
        ToolDefinition::function(
            TOOL_RESUME_AGENT,
            "Resume a closed agent by recreating or reattaching its Docker container.",
            object_schema(vec![
                ("id", json!({ "type": "string" }), false),
                ("agent_id", json!({ "type": "string" }), false),
            ]),
        ),
    ]
}
