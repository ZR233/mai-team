use mai_protocol::ToolDefinition;
use pl_core::ContainerToolKind;

pub(crate) fn exposed_definitions() -> Vec<ToolDefinition> {
    vec![
        pl_core_container_definition(ContainerToolKind::ReadFile),
        pl_core_container_definition(ContainerToolKind::ListFiles),
        pl_core_container_definition(ContainerToolKind::SearchFiles),
        pl_core_container_definition(ContainerToolKind::ApplyPatch),
    ]
}

fn pl_core_container_definition(kind: ContainerToolKind) -> ToolDefinition {
    match kind.to_schema() {
        pl_model::ToolSchema::Function {
            name,
            description,
            input_schema,
        } => ToolDefinition::function(name, description, input_schema),
        pl_model::ToolSchema::Custom { name, .. } => {
            panic!("container file tool {name} must be a function tool")
        }
    }
}
