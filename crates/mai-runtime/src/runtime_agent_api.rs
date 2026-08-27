use super::*;

impl AgentRuntime {
    pub(crate) async fn await_agent_durable(&self, agent_id: AgentId, revision: u64) -> Result<()> {
        let thread_id = agent_host::canonical_id(agent_id)?;
        let framework = self.agent_framework.get().ok_or_else(|| {
            RuntimeError::InvalidInput("agent framework is not started".to_string())
        })?;
        framework.host().await_durable(&thread_id, revision).await
    }

    pub async fn update_agent(
        &self,
        agent_id: AgentId,
        request: UpdateAgentRequest,
    ) -> Result<AgentSummary> {
        agents::update_agent(self, agent_id, request).await
    }

    pub async fn cleanup_orphaned_containers(&self) -> Result<Vec<String>> {
        let (active_agent_ids, active_project_ids) = {
            let agents = self.state.agents.read().await;
            let projects = self.state.projects.read().await;
            (
                agents
                    .keys()
                    .map(ToString::to_string)
                    .collect::<HashSet<_>>(),
                projects
                    .keys()
                    .map(ToString::to_string)
                    .collect::<HashSet<_>>(),
            )
        };
        Ok(self
            .deps
            .docker
            .cleanup_orphaned_managed_containers(&active_agent_ids, &active_project_ids)
            .await?)
    }

    pub async fn get_agent(&self, agent_id: AgentId) -> Result<AgentDetail> {
        let agent = self.agent(agent_id).await?;
        let canonical_id = agent_host::canonical_id(agent_id)?;
        let snapshot = self.ensure_framework_agent(agent_id).await?;
        let runtime = agent_host::load_runtime(&self.deps.store, &canonical_id).await?;
        let mut summary = agent.summary.read().await.clone();
        summary.token_usage = agent_host::aggregate_usage(&runtime);
        Ok(AgentDetail {
            thread: agent_host::thread_metadata(&summary, &snapshot),
            summary,
        })
    }

    pub async fn tool_trace(&self, agent_id: AgentId, call_id: String) -> Result<ToolTraceDetail> {
        agents::tool_trace(self, agent_id, call_id).await
    }

    pub async fn tool_output_artifact(
        &self,
        agent_id: AgentId,
        call_id: String,
        artifact_id: String,
    ) -> Result<(ToolOutputArtifactInfo, PathBuf)> {
        agents::tool_output_artifact(self, agent_id, call_id, artifact_id).await
    }

    pub async fn agent_logs(
        &self,
        agent_id: AgentId,
        filter: AgentLogFilter,
    ) -> Result<AgentLogsResponse> {
        agents::agent_logs(self, agent_id, filter).await
    }

    pub async fn tool_traces(
        &self,
        agent_id: AgentId,
        filter: ToolTraceFilter,
    ) -> Result<ToolTraceListResponse> {
        agents::tool_traces(self, agent_id, filter).await
    }

    pub async fn send_message(
        self: &Arc<Self>,
        agent_id: AgentId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        let agent = self.agent(agent_id).await?;
        let summary = agent.summary.read().await.clone();
        let thread_id = agent_host::canonical_id(agent_id)?;
        let framework = self.framework_handle()?;
        let snapshot = self.ensure_framework_agent(agent_id).await?;
        runtime_thread_events::ensure_live_message_target(&summary, &snapshot)?;
        let turn_id = framework
            .submit(
                thread_id.clone(),
                pl_core::AgentSubmitRequest::start(thread_id, message)
                    .with_mail_id(Uuid::new_v4().to_string())
                    .with_metadata(json!({ "skillMentions": skill_mentions })),
            )
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        Ok(turn_id.into_string())
    }

    pub async fn cancel_agent(self: &Arc<Self>, agent_id: AgentId) -> Result<()> {
        self.agent(agent_id).await?;
        let canonical_id = agent_host::canonical_id(agent_id)?;
        let handle = self.framework_handle()?;
        let snapshot = self.ensure_framework_agent(agent_id).await?;
        if let Some(turn_id) = snapshot.active_turn_id().cloned() {
            handle
                .cancel_turn(canonical_id, turn_id)
                .await
                .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        }
        Ok(())
    }

    pub async fn cancel_agent_turn(
        self: &Arc<Self>,
        agent_id: AgentId,
        turn_id: TurnId,
    ) -> Result<()> {
        self.agent(agent_id).await?;
        let canonical_id = agent_host::canonical_id(agent_id)?;
        let handle = self.framework_handle()?;
        let snapshot = self.ensure_framework_agent(agent_id).await?;
        let Some(active_turn_id) = snapshot.active_turn_id().cloned() else {
            return Ok(());
        };
        if active_turn_id.as_str() != turn_id {
            return Ok(());
        }
        handle
            .cancel_turn(canonical_id, active_turn_id)
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))
    }

    pub async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
        agents::delete_agent(self, agent_id).await
    }

    pub(super) async fn cleanup_agent_tool_output_namespace(
        &self,
        agent_id: AgentId,
    ) -> Result<()> {
        let namespace = self
            .artifact_files_root
            .join("tool-output")
            .join(agent_id.to_string());
        match tokio::fs::remove_dir_all(namespace).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) async fn cleanup_tool_output_namespaces(
        &self,
        cutoff: std::time::SystemTime,
        batch_size: usize,
    ) -> Result<usize> {
        let root = self.artifact_files_root.join("tool-output");
        let live_agents = self
            .list_agents()
            .await
            .into_iter()
            .map(|agent| agent.id)
            .collect::<std::collections::HashSet<_>>();
        let mut namespaces = match tokio::fs::read_dir(&root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let mut removed = 0;
        while let Some(namespace) = namespaces.next_entry().await? {
            if removed >= batch_size {
                break;
            }
            let Some(name) = namespace.file_name().to_str().map(ToString::to_string) else {
                tracing::warn!(path = %namespace.path().display(), "tool-output namespace is not valid UTF-8");
                continue;
            };
            let Ok(agent_id) = Uuid::parse_str(&name) else {
                tracing::warn!(path = %namespace.path().display(), "tool-output namespace has an invalid agent id");
                continue;
            };
            if !live_agents.contains(&agent_id) {
                tokio::fs::remove_dir_all(namespace.path()).await?;
                removed += 1;
                continue;
            }
            let mut calls = match tokio::fs::read_dir(namespace.path()).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            while removed < batch_size {
                let Some(call) = calls.next_entry().await? else {
                    break;
                };
                if call.metadata().await?.modified()? < cutoff {
                    if call.file_type().await?.is_dir() {
                        tokio::fs::remove_dir_all(call.path()).await?;
                    } else {
                        tokio::fs::remove_file(call.path()).await?;
                    }
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    pub(super) async fn close_agent(&self, agent_id: AgentId) -> Result<()> {
        agents::close_agent(self, agent_id).await
    }

    pub(super) async fn cleanup_agent_workspace(&self, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        let project_id = agent.summary.read().await.project_id;
        let volume = if let Some(project_id) = project_id {
            let review_context = agent.review_context.read().await.clone();
            if let Some(context) = review_context.as_deref() {
                self.cleanup_project_review_context(project_id, context)
                    .await?;
                *agent.review_context.write().await = None;
            }
            self.workspace_manager
                .cleanup_agent_workspace(project_id, agent_id)
                .await?;
            project_agent_workspace_volume(&project_id.to_string(), &agent_id.to_string())
        } else {
            agent_workspace_volume(&agent_id.to_string())
        };
        self.deps.docker.delete_volume(&volume).await?;
        Ok(())
    }

    pub async fn cancel_task(self: &Arc<Self>, task_id: TaskId) -> Result<()> {
        tasks::cancel_task(&self.state, self, task_id).await
    }

    pub async fn delete_task(self: &Arc<Self>, task_id: TaskId) -> Result<()> {
        tasks::delete_task(&self.state, self, task_id).await
    }

    pub async fn upload_file(
        &self,
        agent_id: AgentId,
        path: String,
        content_base64: String,
    ) -> Result<usize> {
        agents::upload_file(self, agent_id, path, content_base64).await
    }

    pub async fn download_file_tar(&self, agent_id: AgentId, path: String) -> Result<Vec<u8>> {
        agents::download_file_tar(self, agent_id, path).await
    }

    pub async fn save_artifact(
        self: &Arc<Self>,
        agent_id: AgentId,
        path: String,
        display_name: Option<String>,
    ) -> Result<ArtifactInfo> {
        tasks::save_artifact(&self.state, self.as_ref(), agent_id, path, display_name).await
    }

    pub fn artifact_file_path(&self, info: &ArtifactInfo) -> PathBuf {
        tasks::artifact_file_path(&self.artifact_files_root, info)
    }

    pub fn tool_output_artifact_file_path(
        &self,
        agent_id: AgentId,
        call_id: &str,
        artifact_id: &str,
        name: &str,
    ) -> PathBuf {
        let namespace = agent_id.to_string();
        pl_core::tool::output_format::capture::tool_output_artifact_file_path(
            pl_core::tool::output_format::capture::ToolOutputArtifactPathRequest::new(
                &self.artifact_files_root,
                call_id,
                artifact_id,
                name,
            )
            .with_namespace(&namespace),
        )
    }

    pub(super) async fn save_task_plan(
        self: &Arc<Self>,
        agent_id: AgentId,
        title: String,
        markdown: String,
    ) -> Result<TaskSummary> {
        tasks::save_task_plan(&self.state, self.as_ref(), agent_id, title, markdown).await
    }

    pub(super) async fn submit_review_result(
        self: &Arc<Self>,
        agent_id: AgentId,
        passed: bool,
        findings: String,
        summary: String,
    ) -> Result<TaskReview> {
        tasks::submit_review_result(
            &self.state,
            self.as_ref(),
            agent_id,
            passed,
            findings,
            summary,
        )
        .await
    }

    pub(super) fn spawn_task_workflow(self: &Arc<Self>, task_id: TaskId) {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = tasks::run_task_workflow(&runtime.state, &runtime, task_id).await
                && let Ok(task) = runtime.task(task_id).await
            {
                let _ = runtime
                    .set_task_status(&task, TaskStatus::Failed, None, Some(err.to_string()))
                    .await;
            }
        });
    }

    pub(super) async fn spawn_task_role_agent(
        self: &Arc<Self>,
        parent_agent_id: AgentId,
        role: AgentRole,
        name: Option<String>,
    ) -> Result<AgentSummary> {
        self.ensure_framework_agent(parent_agent_id).await?;
        let parent_runtime_id = agent_host::canonical_id(parent_agent_id)?;
        let child_id = AgentId::new_v4();
        let thread_id = pl_core::ThreadId::new(child_id.to_string())?;
        let result = self
            .framework_handle()?
            .spawn(pl_core::AgentSpawnRequest {
                thread_id,
                parent_id: parent_runtime_id,
                role: pl_core::AgentRoleId::new(role.to_string())?,
                session: pl_core::ThreadContextState::empty(),
                initial_turn_id: None,
                initial_message: None,
                metadata: json!({ "name": name.clone(), "taskName": name }),
            })
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        let (_, child) = agent_host::product_agent(self, &result.snapshot.identity.id).await?;
        let summary = child.summary.read().await.clone();
        Ok(summary)
    }

    pub(super) async fn start_agent_turn(
        self: &Arc<Self>,
        agent_id: AgentId,
        message: String,
    ) -> Result<TurnId> {
        self.send_message(agent_id, message, Vec::new()).await
    }

    pub(super) async fn wait_agent(
        &self,
        agent_id: AgentId,
        timeout: Duration,
    ) -> Result<AgentSummary> {
        let agent = self.agent(agent_id).await?;
        let canonical_id = agent_host::canonical_id(agent_id)?;
        self.ensure_framework_agent(agent_id).await?;
        tokio::time::timeout(
            timeout,
            self.framework_handle()?.wait_until_idle(canonical_id),
        )
        .await
        .map_err(|_| RuntimeError::InvalidInput("waiting for agent timed out".to_string()))?
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        let summary = agent.summary.read().await.clone();
        Ok(summary)
    }

    pub(super) async fn wait_agent_until_complete_with_cancel(
        &self,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> Result<pl_core::AgentWaitResult> {
        self.agent(agent_id).await?;
        let canonical_id = agent_host::canonical_id(agent_id)?;
        self.ensure_framework_agent(agent_id).await?;
        let handle = self.framework_handle()?;
        tokio::select! {
            result = handle.wait_until_idle(canonical_id) => {
                result.map_err(|error| RuntimeError::InvalidInput(error.to_string()))
            }
            () = cancellation_token.cancelled() => Err(RuntimeError::TurnCancelled),
        }
    }
}
