use super::*;

impl agents::AgentCloseOps for AgentRuntime {
    async fn agent(&self, agent_id: AgentId) -> Result<Arc<AgentRecord>> {
        AgentRuntime::agent(self, agent_id).await
    }

    /// 只停止（不删除）执行环境，并撤销该 Agent 的固定环境租约。
    ///
    /// 容器、`agent_reference_bindings` 与工作区数据全部保留：后续 `ensure` 会按同一 binding
    /// 重新核验并复用同一环境，而不是退回基础镜像重建。容器已经不存在时视为已完成，
    /// 因此这里既不代以删除，也不代以重建。
    async fn release_agent_environment(
        &self,
        agent_id: AgentId,
        container_id: Option<String>,
    ) -> Result<Vec<String>> {
        let drain = self.references.leases.remove(agent_id).await;
        let stopped = match container_id {
            Some(container_id) => {
                self.deps.docker.stop_container(&container_id).await?;
                vec![container_id]
            }
            None => Vec::new(),
        };
        drain?;
        Ok(stopped)
    }
}

impl agents::AgentPurgeOps for AgentRuntime {
    async fn agent_summaries(&self) -> Vec<AgentSummary> {
        AgentRuntime::list_agents(self).await
    }

    async fn delete_agent_from_store(&self, agent_id: AgentId) -> Result<()> {
        self.deps.store.delete_agent(agent_id).await?;
        self.references.reclaim_snapshot_resources().await?;
        Ok(())
    }

    async fn delete_agent_artifacts(&self, agent_id: AgentId) -> Result<()> {
        self.cleanup_agent_tool_output_namespace(agent_id).await
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

impl agents::AgentDeleteOps for AgentRuntime {
    fn agent_admission(&self, agent_id: AgentId) -> Arc<mai_references::AgentAdmission> {
        self.references.admissions.agent(agent_id)
    }

    async fn close_canonical_agent(&self, agent_id: AgentId) -> Result<()> {
        // 只撤销当前 agent 的订阅；typed close 语义由 `close_thread` 保留，关闭失败时
        // 驻留 owner 仍在，调用方可对同一 id 重试。
        self.state.thread_subscriptions.invalidate(agent_id).await;
        self.close_thread(agent_id).await
    }

    /// 撤销并移除环境租约，使残存句柄立即失效；容器删除留给运行资源清理。
    async fn revoke_agent_environment_lease(&self, agent_id: AgentId) -> Result<()> {
        let drain = self.references.leases.remove(agent_id).await;
        if drain.is_err()
            && let Some(binding) = self.deps.store.reference_binding(agent_id).await?
        {
            self.deps
                .docker
                .stop_container(&binding.container_id)
                .await?;
        }
        drain?;
        Ok(())
    }

    /// 关闭 MCP sidecar、按精确容器 ID 删除环境容器，然后回收该 Agent 自己的工作区。
    ///
    /// 容器必须先于工作区卷删除：卷还被环境容器挂载时无法回收。任何一步失败都要向上传播，
    /// 让删除停在可重试状态，不假装删除成功；sidecar 在 `shutdown` 内部按 ID 删除失败时，
    /// 下面的 `delete_agent_containers` 仍会覆盖同一批容器，失败不会被吞掉。
    async fn delete_agent_runtime_resources(&self, agent_id: AgentId) -> Result<()> {
        let agent = AgentRuntime::agent(self, agent_id).await?;
        let mcp = agent.mcp.write().await.take();
        if let Some(mcp) = mcp {
            mcp.shutdown().await;
        }
        let preferred_container_id = match self.deps.store.reference_binding(agent_id).await? {
            Some(binding) => Some(binding.container_id),
            None => agent.summary.read().await.container_id.clone(),
        };
        self.deps
            .docker
            .delete_agent_containers(&agent_id.to_string(), preferred_container_id.as_deref())
            .await?;
        *agent.container.write().await = None;
        AgentRuntime::cleanup_agent_workspace(self, agent_id).await
    }
}

impl agents::AgentFileOps for AgentRuntime {
    async fn environment_lease(
        &self,
        agent_id: AgentId,
    ) -> Result<Arc<mai_references::AgentEnvironmentLease>> {
        AgentRuntime::environment_lease(self, agent_id).await
    }

    async fn copy_to_container(
        &self,
        lease: Arc<mai_references::AgentEnvironmentLease>,
        local_path: PathBuf,
        container_path: String,
    ) -> Result<()> {
        let admission = self.references.admissions.agent(lease.agent_id());
        let _lifecycle = admission.lifecycle.lock().await;
        lease.ensure_active()?;
        if admission.pending_revision().is_some() {
            return Err(mai_references::ReferenceError::new(
                ReferenceErrorCode::EnvironmentNotReady,
                "环境等待应用，暂不接收上传",
            )
            .into());
        }
        let runtime = self
            .self_ref
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| RuntimeError::InvalidInput("Runtime 已关闭".into()))?;
        let backend = turn::container::MaiContainerBackend::new(runtime, Arc::clone(&lease));
        let policy = turn::container::ContainerWritePolicy::references_denied("/");
        let path = backend
            .authorize_container_write(&policy, None, &container_path)
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.message()))?;
        self.deps
            .docker
            .copy_to_container(lease.container_id(), &local_path, &path)
            .await?;
        Ok(())
    }

    async fn copy_from_container_tar(
        &self,
        lease: Arc<mai_references::AgentEnvironmentLease>,
        container_path: String,
    ) -> Result<Vec<u8>> {
        let admission = self.references.admissions.agent(lease.agent_id());
        let _lifecycle = admission.lifecycle.lock().await;
        lease.ensure_active()?;
        if admission.pending_revision().is_some() {
            return Err(mai_references::ReferenceError::new(
                ReferenceErrorCode::EnvironmentNotReady,
                "环境等待应用，暂不接收文件请求",
            )
            .into());
        }
        Ok(self
            .deps
            .docker
            .copy_from_container_tar(lease.container_id(), &container_path)
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
        call_id: String,
    ) -> Result<Option<ToolTraceDetail>> {
        Ok(self
            .deps
            .store
            .load_tool_trace(agent_id, Some(agent_id.to_string()), &call_id)
            .await?)
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
    ) -> Result<pl_model::config::ResolvedModelRoute> {
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
    ) -> Result<pl_model::config::ResolvedModelRoute> {
        self.resolve_role_provider_selection(role, provider_id, model)
            .await
    }

    async fn insert_agent_row(
        &self,
        summary: &AgentSummary,
        system_prompt: Option<&str>,
    ) -> Result<()> {
        self.deps.store.insert_agent(summary, system_prompt).await?;
        Ok(())
    }

    async fn insert_agent(&self, agent: Arc<AgentRecord>) {
        let id = agent.summary.read().await.id;
        self.state.agents.write().await.insert(id, agent);
    }
}

impl agents::AgentContainerOps for AgentRuntime {
    fn environment_admission(&self, agent_id: AgentId) -> Arc<mai_references::AgentAdmission> {
        self.references.admissions.agent(agent_id)
    }
    async fn start_agent_container(
        &self,
        request: agents::AgentContainerStartRequest,
    ) -> Result<ContainerHandle> {
        self.start_reference_environment(request).await
    }

    async fn remove_agent_container(&self, agent_id: AgentId, container_id: String) {
        if let Err(error) = self.references.leases.remove(agent_id).await {
            tracing::error!(%agent_id, %error, "MCP 装配失败后 generation 未收敛，保持阻塞");
        }
        self.references.admissions.agent(agent_id).block(0);
        let _ = self.deps.docker.stop_container(&container_id).await;
    }

    async fn agent_mcp_runtime_config(
        &self,
        agent: &AgentRecord,
    ) -> Result<crate::mcp::ContainerMcpSettings> {
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
        Ok(crate::mcp::ContainerMcpSettings {
            enabled: config.mcp.enabled,
            user_servers,
            builtin_servers: config.mcp.builtin_servers.clone(),
            models: config.models.clone(),
        })
    }

    async fn start_agent_mcp_runtime(
        &self,
        agent_id: AgentId,
        container_id: String,
        config: crate::mcp::ContainerMcpSettings,
    ) -> Result<crate::mcp::ContainerMcpRuntime> {
        let lease = self.references.leases.acquire(agent_id).await?;
        let mounts = mai_references::reference_mounts(lease.references())?;
        crate::mcp::ContainerMcpRuntime::start(
            self.deps.docker.clone(),
            mai_docker::AgentMcpSidecarPlan::new(
                agent_id.to_string(),
                lease.generation(),
                container_id,
                &self.sidecar_image,
                mounts,
            )?,
            config,
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
