use mai_protocol::ToolDefinition;

mod agents;
mod collab;
mod container;
mod files;
mod github;
mod mcp;
mod workflow;

pub(crate) fn builtin_tool_definitions() -> Vec<ToolDefinition> {
    let mut tools = Vec::new();
    tools.extend(container::definitions());
    tools.extend(files::exposed_definitions());
    tools.extend(agents::definitions());
    tools.extend(mcp::definitions());
    tools.extend(workflow::definitions());
    tools.extend(github::definitions());
    tools
}
