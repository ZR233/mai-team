use mai_protocol::ToolDefinition;

mod github;
mod review;
mod workflow;

pub(crate) fn builtin_tool_definitions() -> Vec<ToolDefinition> {
    let mut tools = Vec::new();
    tools.extend(workflow::definitions());
    tools.extend(github::definitions());
    tools.extend(review::definitions());
    tools
}
