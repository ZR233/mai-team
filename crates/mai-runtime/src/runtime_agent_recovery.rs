use super::*;

impl agents::AgentResourceRecoveryOps for AgentRuntime {
    async fn close_agent_resources(&self, agent_id: AgentId) -> Result<()> {
        self.close_agent(agent_id).await
    }

    async fn prepare_agent_workspace(
        &self,
        agent_id: AgentId,
    ) -> Result<agents::PreparedAgentWorkspace> {
        let agent = self.agent(agent_id).await?;
        let project_id = agent.summary.read().await.project_id.ok_or_else(|| {
            RuntimeError::InvalidInput(
                "project agent resource recovery requires a project".to_string(),
            )
        })?;
        let project = self.project(project_id).await?;
        let project_summary = project.summary.read().await.clone();
        if project_summary.status != ProjectStatus::Ready
            || project_summary.clone_status != ProjectCloneStatus::Ready
        {
            return Err(RuntimeError::InvalidInput(format!(
                "project `{project_id}` is not ready for agent resource recovery"
            )));
        }
        let source = self
            .agent_container_source_for_project(
                agent_id,
                Some(project_id),
                agents::ContainerSource::FreshImage,
            )
            .await?;
        Ok(agents::PreparedAgentWorkspace { source })
    }

    async fn start_agent_container(
        &self,
        agent_id: AgentId,
        workspace: agents::PreparedAgentWorkspace,
    ) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        agents::ensure_agent_container_with_source(self, &agent, &workspace.source)
            .await
            .map(|_| ())
    }

    async fn cleanup_agent_workspace(&self, agent_id: AgentId) -> Result<()> {
        AgentRuntime::cleanup_agent_workspace(self, agent_id).await
    }

    async fn mark_recovery_failed(&self, agent_id: AgentId, error: String) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        self.set_agent_resource_state(&agent, AgentResourceState::Failed, Some(error))
            .await
    }
}

impl AgentRuntime {
    pub(super) async fn recover_project_agent_resources(
        self: &Arc<Self>,
        request: agents::AgentResourceRecoveryRequest,
    ) -> Result<()> {
        agents::recover_agent_resources(Arc::clone(self), request).await
    }
}
