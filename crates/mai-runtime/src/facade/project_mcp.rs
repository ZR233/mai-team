use mai_protocol::ProjectId;

use crate::{AgentRuntime, Result, projects};

impl AgentRuntime {
    pub(crate) async fn delete_project_sidecar(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<String>> {
        projects::mcp::delete_sidecar(&self.state, &self.deps.docker, project_id).await
    }
}
