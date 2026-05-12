use mai_mcp::McpTool;
use mai_protocol::ToolDefinition;
use serde_json::{Value, json};

pub const TOOL_CONTAINER_EXEC: &str = "container_exec";
pub const TOOL_CONTAINER_CP_UPLOAD: &str = "container_cp_upload";
pub const TOOL_CONTAINER_CP_DOWNLOAD: &str = "container_cp_download";
pub const TOOL_SPAWN_AGENT: &str = "spawn_agent";
pub const TOOL_SEND_INPUT: &str = "send_input";
pub const TOOL_SEND_MESSAGE: &str = "send_message";
pub const TOOL_WAIT_AGENT: &str = "wait_agent";
pub const TOOL_LIST_AGENTS: &str = "list_agents";
pub const TOOL_CLOSE_AGENT: &str = "close_agent";
pub const TOOL_RESUME_AGENT: &str = "resume_agent";
pub const TOOL_LIST_MCP_RESOURCES: &str = "list_mcp_resources";
pub const TOOL_LIST_MCP_RESOURCE_TEMPLATES: &str = "list_mcp_resource_templates";
pub const TOOL_READ_MCP_RESOURCE: &str = "read_mcp_resource";
pub const TOOL_SAVE_TASK_PLAN: &str = "save_task_plan";
pub const TOOL_SUBMIT_REVIEW_RESULT: &str = "submit_review_result";
pub const TOOL_UPDATE_TODO_LIST: &str = "update_todo_list";
pub const TOOL_REQUEST_USER_INPUT: &str = "request_user_input";
pub const TOOL_SAVE_ARTIFACT: &str = "save_artifact";
pub const TOOL_GITHUB_API_GET: &str = "github_api_get";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutedTool {
    ContainerExec,
    ContainerCpUpload,
    ContainerCpDownload,
    SpawnAgent,
    SendInput,
    SendMessage,
    WaitAgent,
    ListAgents,
    CloseAgent,
    ResumeAgent,
    ListMcpResources,
    ListMcpResourceTemplates,
    ReadMcpResource,
    SaveTaskPlan,
    SubmitReviewResult,
    UpdateTodoList,
    RequestUserInput,
    SaveArtifact,
    GithubApiGet,
    Mcp(String),
    Unknown(String),
}

pub fn route_tool(name: &str) -> RoutedTool {
    match normalize_name(name).as_str() {
        TOOL_CONTAINER_EXEC => RoutedTool::ContainerExec,
        TOOL_CONTAINER_CP_UPLOAD => RoutedTool::ContainerCpUpload,
        TOOL_CONTAINER_CP_DOWNLOAD => RoutedTool::ContainerCpDownload,
        TOOL_SPAWN_AGENT => RoutedTool::SpawnAgent,
        TOOL_SEND_INPUT => RoutedTool::SendInput,
        TOOL_SEND_MESSAGE => RoutedTool::SendMessage,
        TOOL_WAIT_AGENT => RoutedTool::WaitAgent,
        TOOL_LIST_AGENTS => RoutedTool::ListAgents,
        TOOL_CLOSE_AGENT => RoutedTool::CloseAgent,
        TOOL_RESUME_AGENT => RoutedTool::ResumeAgent,
        TOOL_LIST_MCP_RESOURCES => RoutedTool::ListMcpResources,
        TOOL_LIST_MCP_RESOURCE_TEMPLATES => RoutedTool::ListMcpResourceTemplates,
        TOOL_READ_MCP_RESOURCE => RoutedTool::ReadMcpResource,
        TOOL_SAVE_TASK_PLAN => RoutedTool::SaveTaskPlan,
        TOOL_SUBMIT_REVIEW_RESULT => RoutedTool::SubmitReviewResult,
        TOOL_UPDATE_TODO_LIST => RoutedTool::UpdateTodoList,
        TOOL_REQUEST_USER_INPUT => RoutedTool::RequestUserInput,
        TOOL_SAVE_ARTIFACT => RoutedTool::SaveArtifact,
        TOOL_GITHUB_API_GET => RoutedTool::GithubApiGet,
        normalized if normalized.starts_with("mcp__") => RoutedTool::Mcp(normalized.to_string()),
        normalized => RoutedTool::Unknown(normalized.to_string()),
    }
}

pub fn build_tool_definitions(mcp_tools: &[McpTool]) -> Vec<ToolDefinition> {
    build_tool_definitions_with_filter(mcp_tools, |_| true)
}

pub fn build_tool_definitions_with_filter(
    mcp_tools: &[McpTool],
    allow_tool: impl Fn(&str) -> bool,
) -> Vec<ToolDefinition> {
    let mut tools = builtin_tool_definitions()
        .into_iter()
        .filter(|tool| allow_tool(&tool.name))
        .collect::<Vec<_>>();
    tools.extend(
        mcp_tools
            .iter()
            .filter(|tool| allow_tool(&tool.model_name))
            .map(|tool| {
                ToolDefinition::function(
                    tool.model_name.clone(),
                    if tool.description.is_empty() {
                        format!("Call MCP tool `{}` on server `{}`.", tool.name, tool.server)
                    } else {
                        tool.description.clone()
                    },
                    tool.input_schema.clone(),
                )
            }),
    );
    tools
}

fn builtin_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            TOOL_CONTAINER_EXEC,
            "Execute a shell command inside this agent's Docker container. timeout_secs is optional; omit it for no command time limit.",
            object_schema(vec![
                ("command", json!({ "type": "string" }), true),
                ("cwd", json!({ "type": "string" }), false),
                (
                    "timeout_secs",
                    json!({ "type": "integer", "minimum": 1 }),
                    false,
                ),
            ]),
        ),
        ToolDefinition::function(
            TOOL_CONTAINER_CP_UPLOAD,
            "Write a base64 encoded file into this agent's Docker container.",
            object_schema(vec![
                ("path", json!({ "type": "string" }), true),
                ("content_base64", json!({ "type": "string" }), true),
            ]),
        ),
        ToolDefinition::function(
            TOOL_CONTAINER_CP_DOWNLOAD,
            "Export a file or directory from this agent's Docker container as a base64 encoded tar stream.",
            object_schema(vec![("path", json!({ "type": "string" }), true)]),
        ),
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
        ToolDefinition::function(
            TOOL_LIST_MCP_RESOURCES,
            "Lists resources provided by MCP servers.",
            object_schema(vec![
                ("server", json!({ "type": "string" }), false),
                ("cursor", json!({ "type": "string" }), false),
            ]),
        ),
        ToolDefinition::function(
            TOOL_LIST_MCP_RESOURCE_TEMPLATES,
            "Lists resource templates provided by MCP servers.",
            object_schema(vec![
                ("server", json!({ "type": "string" }), false),
                ("cursor", json!({ "type": "string" }), false),
            ]),
        ),
        ToolDefinition::function(
            TOOL_READ_MCP_RESOURCE,
            "Read a specific resource from an MCP server.",
            object_schema(vec![
                ("server", json!({ "type": "string" }), true),
                ("uri", json!({ "type": "string" }), true),
            ]),
        ),
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
        ToolDefinition::function(
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
        ),
    ]
}

fn object_schema(fields: Vec<(&str, Value, bool)>) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, schema, is_required) in fields {
        properties.insert(name.to_string(), schema);
        if is_required {
            required.push(Value::String(name.to_string()));
        }
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn collab_items_schema() -> Value {
    json!({
        "type": "array",
        "items": {
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "type": { "type": "string", "enum": ["text"] },
                        "text": { "type": "string" }
                    },
                    "required": ["type", "text"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "type": { "type": "string", "enum": ["skill"] },
                        "name": { "type": "string" },
                        "path": { "type": "string" }
                    },
                    "required": ["type"],
                    "anyOf": [
                        { "required": ["name"] },
                        { "required": ["path"] }
                    ],
                    "additionalProperties": false
                }
            ]
        }
    })
}

fn normalize_name(name: &str) -> String {
    name.replace('.', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_dot_aliases() {
        assert_eq!(route_tool("container.exec"), RoutedTool::ContainerExec);
        assert_eq!(route_tool("container_exec"), RoutedTool::ContainerExec);
    }

    #[test]
    fn builds_builtin_definitions() {
        let tools = build_tool_definitions(&[]);
        assert!(tools.iter().any(|tool| tool.name == TOOL_SPAWN_AGENT));
        assert!(tools.iter().all(|tool| tool.kind == "function"));
    }

    #[test]
    fn spawn_agent_schema_does_not_expose_model_selection() {
        let tools = build_tool_definitions(&[]);
        let spawn = tools
            .iter()
            .find(|tool| tool.name == TOOL_SPAWN_AGENT)
            .expect("spawn tool");
        let properties = spawn
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert!(properties.contains_key("name"));
        assert!(properties.contains_key("message"));
        assert!(properties.contains_key("role"));
        assert!(properties.contains_key("agent_type"));
        assert!(properties.contains_key("model"));
        assert!(!properties.contains_key("provider_id"));
    }

    #[test]
    fn collab_items_schema_accepts_skill_items() {
        let tools = build_tool_definitions(&[]);
        for tool_name in [TOOL_SPAWN_AGENT, TOOL_SEND_INPUT] {
            let tool = tools
                .iter()
                .find(|tool| tool.name == tool_name)
                .expect("tool");
            let item_variants = tool
                .parameters
                .pointer("/properties/items/items/oneOf")
                .and_then(Value::as_array)
                .expect("oneOf item variants");
            assert_eq!(item_variants.len(), 2);
            let skill_variant = item_variants
                .iter()
                .find(|variant| {
                    variant
                        .pointer("/properties/type/enum/0")
                        .and_then(Value::as_str)
                        == Some("skill")
                })
                .expect("skill variant");
            assert!(
                skill_variant
                    .get("anyOf")
                    .and_then(Value::as_array)
                    .is_some_and(|items| items.len() == 2)
            );
        }
    }

    #[test]
    fn update_todo_list_schema_uses_items_field() {
        let tools = build_tool_definitions(&[]);
        let update_todo = tools
            .iter()
            .find(|tool| tool.name == TOOL_UPDATE_TODO_LIST)
            .expect("update_todo_list tool");
        let properties = update_todo
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert!(properties.contains_key("items"));
        assert!(!properties.contains_key("todos"));
        assert_eq!(
            update_todo.parameters.get("required"),
            Some(&json!(["items"]))
        );
    }

    #[test]
    fn codex_compatible_subagent_tools_are_exposed() {
        let tools = build_tool_definitions(&[]);
        let names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&TOOL_SEND_INPUT));
        assert!(names.contains(&TOOL_WAIT_AGENT));
        assert!(names.contains(&TOOL_CLOSE_AGENT));
        assert!(names.contains(&TOOL_RESUME_AGENT));
        let wait = tools
            .iter()
            .find(|tool| tool.name == TOOL_WAIT_AGENT)
            .expect("wait_agent");
        let properties = wait
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .expect("wait properties");
        assert!(properties.contains_key("targets"));
        assert!(properties.contains_key("timeout_ms"));
        assert!(properties.contains_key("agent_id"));
        assert_eq!(properties["timeout_ms"]["type"].as_str(), Some("integer"));
        assert_eq!(properties["timeout_ms"]["minimum"].as_u64(), Some(100));
        assert!(properties["timeout_ms"].get("maximum").is_none());
        assert_eq!(properties["timeout_secs"]["type"].as_str(), Some("integer"));
        assert_eq!(properties["timeout_secs"]["minimum"].as_u64(), Some(1));
        assert!(properties["timeout_secs"].get("maximum").is_none());
        assert_eq!(route_tool("send_input"), RoutedTool::SendInput);
        assert_eq!(route_tool("resume_agent"), RoutedTool::ResumeAgent);
        assert_eq!(route_tool("github_api_get"), RoutedTool::GithubApiGet);
    }

    #[test]
    fn container_exec_timeout_is_optional_without_budget_cap() {
        let tools = build_tool_definitions(&[]);
        let exec = tools
            .iter()
            .find(|tool| tool.name == TOOL_CONTAINER_EXEC)
            .expect("container_exec");
        let timeout = exec
            .parameters
            .pointer("/properties/timeout_secs")
            .expect("timeout_secs schema");
        assert_eq!(timeout["type"].as_str(), Some("integer"));
        assert_eq!(timeout["minimum"].as_u64(), Some(1));
        assert!(timeout.get("maximum").is_none());
        assert!(
            !exec
                .parameters
                .pointer("/required")
                .and_then(Value::as_array)
                .is_some_and(|required| required.iter().any(|item| item == "timeout_secs"))
        );
    }
}
