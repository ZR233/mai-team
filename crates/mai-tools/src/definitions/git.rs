use crate::names::{TOOL_GIT_SYNC_DEFAULT_BRANCH, TOOL_GIT_WORKTREE_INFO};
use crate::schema::object_schema;
use mai_protocol::ToolDefinition;
use pl_core::{
    GitTool, GitToolKind, GitWorkspaceConfig, LocalExecutionBackend, NoGitCredentialProvider, Tool,
};
use serde_json::json;
use std::sync::Arc;

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    let mut definitions = GitToolKind::all()
        .iter()
        .map(|kind| pl_core_git_definition(*kind))
        .collect::<Vec<_>>();
    definitions.extend([
        ToolDefinition::function(
            TOOL_GIT_WORKTREE_INFO,
            "Show compatibility information about this project agent git workspace clone.",
            object_schema(vec![]),
        ),
        ToolDefinition::function(
            TOOL_GIT_SYNC_DEFAULT_BRANCH,
            "Sync this project agent workspace clone with the project's latest default branch.",
            object_schema(vec![
                ("force", json!({ "type": "boolean" }), false),
                ("preserve_changes", json!({ "type": "boolean" }), false),
            ]),
        ),
    ]);
    definitions
}

fn pl_core_git_definition(kind: GitToolKind) -> ToolDefinition {
    let tool = GitTool::new(
        kind,
        GitWorkspaceConfig::local(std::env::temp_dir()),
        Arc::new(LocalExecutionBackend),
        Arc::new(NoGitCredentialProvider),
    );
    match tool.to_schema() {
        pl_model::ToolSchema::Function {
            name,
            description,
            input_schema,
        } => ToolDefinition::function(name, description, input_schema),
        pl_model::ToolSchema::Custom { name, .. } => {
            panic!("pl-core git tool `{name}` must be a function tool")
        }
    }
}
