use super::*;

impl AgentRuntime {
    pub(super) async fn clone_project_repository(
        &self,
        project_id: ProjectId,
        maintainer_agent_id: AgentId,
    ) -> Result<()> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let account_id = summary.git_account_id.clone().ok_or_else(|| {
            RuntimeError::InvalidInput("project git account is not configured".to_string())
        })?;
        let token = self.deps.git_accounts.token(&account_id).await?;
        self.ensure_project_cache_volume(project_id).await?;
        self.sync_agent_workspace_volume_repo(&summary, maintainer_agent_id, &token)
            .await?;
        self.refresh_project_skill_cache_from_project_repository(project_id)
            .await?;
        Ok(())
    }

    pub(super) async fn set_task_status(
        &self,
        task: &Arc<TaskRecord>,
        status: TaskStatus,
        final_report: Option<String>,
        error: Option<String>,
    ) -> Result<()> {
        tasks::set_status(&self.state, self, task, status, final_report, error).await
    }

    pub(super) async fn resolve_session_id(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> Result<agent_host::ResolvedAgentSessionId> {
        let agent = self.agent(agent_id).await?;
        let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
        let runtime = agent_host::load_runtime(&self.deps.store, &runtime_agent_id).await?;
        let sessions = agent_host::project_sessions(&runtime);
        let selected = agent_host::selected_session(&sessions, session_id).ok_or_else(|| {
            RuntimeError::SessionNotFound {
                agent_id,
                session_id: session_id.unwrap_or_default(),
            }
        })?;
        Ok(agent_host::ResolvedAgentSessionId {
            protocol: selected.summary.id,
            framework: pl_core::SessionId::new(selected.framework_id.clone())?,
        })
    }

    pub(super) async fn resolve_role_agent_model(
        &self,
        role: AgentRole,
    ) -> Result<ResolvedAgentModel> {
        let config = config::agent_config_from_models(&self.mai_config.read().await.models);
        let preference = role_preference(&config, role);
        match self.resolve_agent_model_preference(role, preference).await {
            Ok(resolved) => Ok(resolved),
            Err(err) if preference.is_some() && is_stale_agent_model_selection_error(&err) => {
                tracing::warn!(
                    role = agent_role_label(role),
                    error = %err,
                    "agent role model preference is stale; falling back to the default provider"
                );
                self.resolve_agent_model_preference(role, None).await
            }
            Err(err) => Err(err),
        }
    }

    pub(super) async fn resolve_effective_agent_model(
        &self,
        role: AgentRole,
        preference: Option<&AgentModelPreference>,
        validation_errors: &mut Vec<String>,
    ) -> Option<ResolvedAgentModelPreference> {
        match self.resolve_agent_model_preference(role, preference).await {
            Ok(resolved) => Some(resolved.effective),
            Err(err) => {
                validation_errors.push(err.to_string());
                None
            }
        }
    }

    pub(super) async fn resolve_agent_model_preference(
        &self,
        role: AgentRole,
        preference: Option<&AgentModelPreference>,
    ) -> Result<ResolvedAgentModel> {
        if let Some(preference) = preference
            && (preference.provider_id.trim().is_empty() || preference.model.trim().is_empty())
        {
            return Err(RuntimeError::InvalidInput(format!(
                "{} provider and model are required",
                agent_role_label(role)
            )));
        }
        let selection = self
            .resolve_provider_selection(
                preference.map(|item| item.provider_id.as_str()),
                preference.map(|item| item.model.as_str()),
            )
            .await?;
        let reasoning_effort = agents::normalize_reasoning_effort(
            &selection.model,
            preference.and_then(|item| item.reasoning_effort.as_deref()),
            true,
        )?;
        Ok(resolved_agent_model(selection, reasoning_effort))
    }

    pub(super) async fn agent(&self, agent_id: AgentId) -> Result<Arc<AgentRecord>> {
        self.state
            .agents
            .read()
            .await
            .get(&agent_id)
            .cloned()
            .ok_or(RuntimeError::AgentNotFound(agent_id))
    }

    pub(super) async fn container_id(&self, agent_id: AgentId) -> Result<String> {
        let agent = self.agent(agent_id).await?;
        if let Some(container_id) = agent
            .container
            .read()
            .await
            .as_ref()
            .map(|container| container.id.clone())
        {
            return Ok(container_id);
        }
        agents::ensure_agent_container(self, &agent).await
    }

    pub(super) async fn agent_mcp_tools(&self, agent: &AgentRecord) -> Vec<crate::mcp::McpTool> {
        let project_id = agent.summary.read().await.project_id;
        if let Some(project_id) = project_id {
            if projects::mcp::project_mcp_configs("").is_empty() {
                return Vec::new();
            }
            let manager = self
                .state
                .project_mcp_managers
                .read()
                .await
                .get(&project_id)
                .map(projects::mcp::ProjectMcpManagerHandle::manager);
            let Some(manager) = manager else {
                return Vec::new();
            };
            return manager
                .tools()
                .await
                .into_iter()
                .filter(|tool| !projects::mcp::is_github_mcp_tool(tool))
                .collect();
        }
        let Some(manager) = agent.mcp.read().await.clone() else {
            return Vec::new();
        };
        manager.tools().await
    }

    pub(super) async fn inject_project_mcp_tools(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        _session_id: SessionId,
        cancellation_token: &CancellationToken,
    ) -> Result<()> {
        turn::cancellation::ensure_not_cancelled(cancellation_token)?;
        if agent.summary.read().await.project_id.is_none() {
            return Ok(());
        }
        let _ = self
            .project_mcp_manager_for_agent(agent, agent_id, cancellation_token)
            .await?;
        Ok(())
    }
}
