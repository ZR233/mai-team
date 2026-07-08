use pl_model::ToolSchema;

mod github;
mod review;
mod workflow;

pub(crate) fn builtin_tool_schemas() -> Vec<ToolSchema> {
    let mut tools = Vec::new();
    tools.extend(workflow::definitions());
    tools.extend(github::definitions());
    tools.extend(review::definitions());
    tools
}
