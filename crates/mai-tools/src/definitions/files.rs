use mai_protocol::ToolDefinition;
use pl_core::WorkspaceFileToolKind;

pub(crate) fn exposed_definitions() -> Vec<ToolDefinition> {
    vec![
        pl_core_file_definition(WorkspaceFileToolKind::ReadFile),
        pl_core_file_definition(WorkspaceFileToolKind::ListFiles),
        pl_core_file_definition(WorkspaceFileToolKind::SearchFiles),
        pl_core_file_definition(WorkspaceFileToolKind::ApplyPatch),
    ]
}

fn pl_core_file_definition(kind: WorkspaceFileToolKind) -> ToolDefinition {
    match kind.to_schema() {
        pl_model::ToolSchema::Function {
            name,
            description,
            input_schema,
        } => ToolDefinition::function(name, description, input_schema),
        pl_model::ToolSchema::Custom { name, .. } => {
            panic!("workspace file tool {name} must be a function tool")
        }
    }
}
