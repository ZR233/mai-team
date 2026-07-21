use super::*;

impl agents::AgentCloseOps for AgentRuntime {
    async fn agent(&self, agent_id: AgentId) -> Result<Arc<AgentRecord>> {
        AgentRuntime::agent(self, agent_id).await
    }

    async fn persist_agent(&self, agent: &AgentRecord) -> Result<()> {
        AgentRuntime::persist_agent(self, agent).await
    }

    async fn delete_agent_containers(
        &self,
        agent_id: AgentId,
        preferred_container_id: Option<String>,
    ) -> Result<Vec<String>> {
        Ok(self
            .deps
            .docker
            .delete_agent_containers(&agent_id.to_string(), preferred_container_id.as_deref())
            .await?)
    }
}

impl agents::AgentPurgeOps for AgentRuntime {
    async fn agent_summaries(&self) -> Vec<AgentSummary> {
        AgentRuntime::list_agents(self).await
    }

    async fn delete_agent_from_store(&self, agent_id: AgentId) -> Result<()> {
        self.deps.store.delete_agent(agent_id).await?;
        Ok(())
    }

    async fn remove_agent_from_memory(&self, agent_id: AgentId) {
        self.state.agents.write().await.remove(&agent_id);
    }

    async fn publish_agent_deleted(&self, agent_id: AgentId) {
        self.events
            .publish(MaiProductEventKind::AgentDeleted { agent_id })
            .await;
    }
}

impl agents::AgentFileOps for AgentRuntime {
    fn container_id(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<String>> + Send {
        AgentRuntime::container_id(self, agent_id)
    }

    async fn copy_to_container(
        &self,
        container_id: String,
        local_path: PathBuf,
        container_path: String,
    ) -> Result<()> {
        self.deps
            .docker
            .copy_to_container(&container_id, &local_path, &container_path)
            .await?;
        Ok(())
    }

    async fn copy_from_container_tar(
        &self,
        container_id: String,
        container_path: String,
    ) -> Result<Vec<u8>> {
        Ok(self
            .deps
            .docker
            .copy_from_container_tar(&container_id, &container_path)
            .await?)
    }
}

impl agents::AgentObservabilityOps for AgentRuntime {
    fn agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<Arc<AgentRecord>>> + Send {
        AgentRuntime::agent(self, agent_id)
    }

    async fn load_tool_trace(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        call_id: String,
    ) -> Result<Option<ToolTraceDetail>> {
        Ok(self
            .deps
            .store
            .load_tool_trace(agent_id, session_id, &call_id)
            .await?)
    }

    async fn tool_metadata(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        call_id: String,
    ) -> (Option<bool>, Option<u64>) {
        match self
            .deps
            .store
            .load_tool_trace(agent_id, Some(session_id), &call_id)
            .await
        {
            Ok(Some(trace)) => (Some(trace.success), trace.duration_ms),
            Ok(None) => (None, None),
            Err(error) => {
                tracing::warn!(%agent_id, %call_id, "failed to load tool metadata: {error}");
                (None, None)
            }
        }
    }

    async fn list_agent_logs(
        &self,
        agent_id: AgentId,
        filter: AgentLogFilter,
    ) -> Result<Vec<AgentLogEntry>> {
        Ok(self.deps.store.list_agent_logs(agent_id, filter).await?)
    }

    async fn list_tool_traces(
        &self,
        agent_id: AgentId,
        filter: ToolTraceFilter,
    ) -> Result<Vec<ToolTraceSummary>> {
        Ok(self.deps.store.list_tool_traces(agent_id, filter).await?)
    }

    async fn load_agent_history(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<Vec<pl_protocol::Message>> {
        let agent = self.agent(agent_id).await?;
        let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
        let runtime = agent_host::load_runtime(&self.deps.store, &runtime_agent_id).await?;
        agent_host::history_messages(&runtime, session_id)?.ok_or(RuntimeError::SessionNotFound {
            agent_id,
            session_id,
        })
    }

    fn resolve_session_id(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> impl std::future::Future<Output = Result<agent_host::ResolvedAgentSessionId>> + Send {
        AgentRuntime::resolve_session_id(self, agent_id, session_id)
    }

    fn tool_output_artifact_file_path(
        &self,
        agent_id: AgentId,
        call_id: &str,
        artifact_id: &str,
        name: &str,
    ) -> PathBuf {
        AgentRuntime::tool_output_artifact_file_path(self, agent_id, call_id, artifact_id, name)
    }
}

impl agents::AgentResourceBrokerOps for AgentRuntime {
    fn project_skill_read_guard(
        &self,
        agent: &AgentRecord,
    ) -> impl std::future::Future<Output = Option<tokio::sync::OwnedRwLockReadGuard<()>>> + Send
    {
        AgentRuntime::project_skill_read_guard(self, agent)
    }

    async fn skills_config(&self) -> Result<SkillsConfigRequest> {
        Ok(self.deps.store.load_skills_config().await?)
    }

    fn skills_manager_for_agent(
        &self,
        agent: &AgentRecord,
    ) -> impl std::future::Future<Output = Result<SkillsManager>> + Send {
        AgentRuntime::skills_manager_for_agent(self, agent)
    }
}

impl agents::AgentUpdateOps for AgentRuntime {
    fn agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<Arc<AgentRecord>>> + Send {
        AgentRuntime::agent(self, agent_id)
    }

    async fn resolve_provider(
        &self,
        role: AgentRole,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<ProviderSelection> {
        self.resolve_role_provider_selection(role, provider_id, model)
            .await
    }

    async fn persist_agent(&self, agent: Arc<AgentRecord>) -> Result<()> {
        AgentRuntime::persist_agent(self, &agent).await
    }

    async fn publish_agent_updated(&self, agent: AgentSummary) {
        self.events
            .publish(MaiProductEventKind::AgentUpdated { agent })
            .await;
    }
}

impl agents::AgentCreateOps for AgentRuntime {
    fn default_docker_image(&self) -> String {
        self.deps.docker.image().to_string()
    }

    async fn resolve_provider(
        &self,
        role: AgentRole,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<ProviderSelection> {
        self.resolve_role_provider_selection(role, provider_id, model)
            .await
    }

    async fn save_agent(&self, summary: &AgentSummary, system_prompt: Option<&str>) -> Result<()> {
        self.deps.store.save_agent(summary, system_prompt).await?;
        Ok(())
    }

    async fn insert_agent(&self, agent: Arc<AgentRecord>) {
        let id = agent.summary.read().await.id;
        self.state.agents.write().await.insert(id, agent);
    }

    async fn publish_agent_created(&self, agent: AgentSummary) {
        self.events
            .publish(MaiProductEventKind::AgentCreated { agent })
            .await;
    }
}

impl agents::AgentContainerOps for AgentRuntime {
    async fn start_agent_container(
        &self,
        request: agents::AgentContainerStartRequest,
    ) -> Result<ContainerHandle> {
        match request.source {
            agents::ContainerSource::FreshImage => Ok(self
                .deps
                .docker
                .ensure_agent_container_from_image(
                    &request.agent_id.to_string(),
                    request.preferred_container_id.as_deref(),
                    &request.docker_image,
                )
                .await?),
            agents::ContainerSource::ProjectReviewWorkspace { .. } => {
                Err(RuntimeError::InvalidInput(
                    "project review workspace source must be resolved before container startup"
                        .to_string(),
                ))
            }
            agents::ContainerSource::ProjectWorkspace {
                workspace_volume,
                repo_path,
                repository_view,
            } => {
                if repo_path != projects::workspace::AGENT_WORKSPACE_REPO_PATH {
                    return Err(RuntimeError::InvalidInput(format!(
                        "unsupported project repo path `{repo_path}`"
                    )));
                }
                self.ensure_agent_workspace_volume(
                    request.agent_id,
                    workspace_volume.as_str(),
                    None,
                )
                .await?;
                let mounts = match repository_view {
                    Some(view) => {
                        if view.container_path
                            != projects::review::context::PROJECT_REPOSITORY_CONTAINER_PATH
                        {
                            return Err(RuntimeError::InvalidInput(format!(
                                "unsupported project repository view path `{}`",
                                view.container_path
                            )));
                        }
                        vec![ContainerVolumeMount::read_only_subpath(
                            view.volume,
                            view.container_path,
                            view.volume_subpath,
                        )?]
                    }
                    None => Vec::new(),
                };
                Ok(self
                    .deps
                    .docker
                    .ensure_agent_container_from_image_with_workspace_and_mounts(
                        &request.agent_id.to_string(),
                        request.preferred_container_id.as_deref(),
                        &request.docker_image,
                        Some(&workspace_volume),
                        &mounts,
                    )
                    .await?)
            }
            agents::ContainerSource::CloneFrom {
                parent_container_id,
                docker_image,
                workspace_volume,
            } => {
                if request.preferred_container_id.is_some() && workspace_volume.is_none() {
                    Ok(self
                        .deps
                        .docker
                        .ensure_agent_container_from_image(
                            &request.agent_id.to_string(),
                            request.preferred_container_id.as_deref(),
                            &docker_image,
                        )
                        .await?)
                } else {
                    if let Some(workspace_volume) = workspace_volume.as_deref() {
                        self.ensure_agent_workspace_volume(
                            request.agent_id,
                            workspace_volume,
                            None,
                        )
                        .await?;
                    }
                    Ok(self
                        .deps
                        .docker
                        .create_agent_container_from_parent_with_workspace_and_mounts(
                            &request.agent_id.to_string(),
                            &parent_container_id,
                            workspace_volume.as_deref(),
                            &[],
                        )
                        .await?)
                }
            }
        }
    }

    async fn remove_agent_container(&self, agent_id: AgentId, container_id: String) {
        let _ = self
            .deps
            .docker
            .delete_agent_containers(&agent_id.to_string(), Some(&container_id))
            .await;
    }

    async fn agent_mcp_runtime_config(
        &self,
        agent: &AgentRecord,
    ) -> Result<agents::AgentMcpRuntimeConfig> {
        let has_project = agent.summary.read().await.project_id.is_some();
        let user_servers = self
            .deps
            .store
            .list_mcp_servers()
            .await?
            .into_iter()
            .filter(|(_, config)| match config.scope {
                McpServerScope::Agent | McpServerScope::System => true,
                McpServerScope::Project => has_project,
            })
            .collect();
        let config = self.mai_config.read().await;
        Ok(agents::AgentMcpRuntimeConfig {
            enabled: config.mcp.enabled,
            user_servers,
            builtin_servers: config.mcp.builtin_servers.clone(),
            models: config.models.clone(),
        })
    }

    async fn start_agent_mcp_runtime(
        &self,
        container_id: String,
        config: agents::AgentMcpRuntimeConfig,
    ) -> Result<crate::mcp::ContainerMcpRuntime> {
        crate::mcp::ContainerMcpRuntime::start(
            self.deps.docker.clone(),
            container_id,
            config.enabled,
            &config.user_servers,
            &config.builtin_servers,
            &config.models,
        )
        .await
    }

    async fn set_agent_resource_state(
        &self,
        agent: Arc<AgentRecord>,
        change: agents::AgentContainerStatusChange,
    ) -> Result<()> {
        AgentRuntime::set_agent_resource_state(self, &agent, change.state, change.error).await
    }

    async fn persist_agent(&self, agent: Arc<AgentRecord>) -> Result<()> {
        AgentRuntime::persist_agent(self, &agent).await
    }

    async fn publish_mcp_status(&self, change: agents::AgentMcpStatusChange) {
        self.events
            .publish(MaiProductEventKind::McpServerStatusChanged {
                agent_id: change.agent_id,
                server: change.server,
                status: change.status,
                error: change.error,
            })
            .await;
    }
}
