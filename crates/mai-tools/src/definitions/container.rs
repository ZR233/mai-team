use mai_protocol::ToolDefinition;
use pl_core::ContainerToolKind;

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    vec![
        pl_core_container_definition(ContainerToolKind::Exec),
        pl_core_container_definition(ContainerToolKind::CopyUpload),
        pl_core_container_definition(ContainerToolKind::CopyDownload),
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
            panic!("container tool {name} must be a function tool")
        }
    }
}
