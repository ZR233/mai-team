use super::*;

impl AgentRuntime {
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

    pub async fn get_agent(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> Result<AgentDetail> {
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
        let mut summary = agent.summary.read().await.clone();
        summary.token_usage = agent_host::aggregate_usage(&runtime);
        Ok(AgentDetail {
            summary,
            sessions: sessions
                .iter()
                .map(|session| session.summary.clone())
                .collect(),
            selected_session_id: selected.summary.id,
        })
    }

    pub async fn create_session(&self, agent_id: AgentId) -> Result<AgentSessionSummary> {
        let agent = self.agent(agent_id).await?;
        let summary = agent.summary.read().await;
        if summary.task_id.is_some()
            && matches!(
                summary.role,
                Some(AgentRole::Explorer | AgentRole::Executor | AgentRole::Reviewer)
            )
        {
            return Err(RuntimeError::InvalidInput(
                "workflow child agents use a single internal task session".to_string(),
            ));
        }
        drop(summary);
        let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
        let runtime = agent_host::load_runtime(&self.deps.store, &runtime_agent_id).await?;
        let session_id = pl_core::SessionId::generate();
        let protocol_session_id = agent_host::protocol_uuid(session_id.as_str());
        let framework_session =
            agent_host::session_state(session_id, format!("Chat {}", runtime.sessions.len() + 1));
        self.framework_handle()?
            .open_session(runtime_agent_id.clone(), framework_session)
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        let runtime = agent_host::load_runtime(&self.deps.store, &runtime_agent_id).await?;
        agent_host::project_sessions(&runtime)
            .into_iter()
            .find(|session| session.summary.id == protocol_session_id)
            .map(|session| session.summary)
            .ok_or_else(|| RuntimeError::SessionNotFound {
                agent_id,
                session_id: protocol_session_id,
            })
    }

    pub async fn tool_trace(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        call_id: String,
    ) -> Result<ToolTraceDetail> {
        agents::tool_trace(self, agent_id, session_id, call_id).await
    }

    pub async fn tool_output_artifact(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        call_id: String,
        artifact_id: String,
    ) -> Result<(ToolOutputArtifactInfo, PathBuf)> {
        agents::tool_output_artifact(self, agent_id, session_id, call_id, artifact_id).await
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
        session_id: Option<SessionId>,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        let session_id = self.resolve_session_id(agent_id, session_id).await?;
        let agent = self.agent(agent_id).await?;
        let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
        let turn_id = self
            .framework_handle()?
            .submit(
                runtime_agent_id,
                pl_core::AgentSubmitRequest::start(session_id.framework, message)
                    .with_metadata(json!({ "skillMentions": skill_mentions })),
            )
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        Ok(agent_host::protocol_uuid(turn_id.as_str()))
    }

    pub async fn cancel_agent(self: &Arc<Self>, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
        let handle = self.framework_handle()?;
        let snapshot = handle
            .snapshot(runtime_agent_id.clone())
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        if let Some(turn_id) = snapshot.active_turn_id {
            handle
                .cancel_turn(runtime_agent_id, turn_id)
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
        let agent = self.agent(agent_id).await?;
        let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
        let handle = self.framework_handle()?;
        let snapshot = handle
            .snapshot(runtime_agent_id.clone())
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        let Some(active_turn_id) = snapshot.active_turn_id else {
            return Ok(());
        };
        if agent_host::protocol_uuid(active_turn_id.as_str()) != turn_id {
            return Ok(());
        }
        handle
            .cancel_turn(runtime_agent_id, active_turn_id)
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))
    }

    pub async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
        self.framework_handle()?
            .close(runtime_agent_id)
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        agents::purge_agent_tree(self, agent_id).await
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
        pl_core::tool_output_artifact_file_path(
            pl_core::ToolOutputArtifactPathRequest::new(
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
        let parent = self.agent(parent_agent_id).await?;
        let parent_runtime_id = parent.runtime_agent_id.read().await.clone();
        let session_id = pl_core::SessionId::generate();
        let result = self
            .framework_handle()?
            .spawn(pl_core::AgentSpawnRequest {
                parent_id: parent_runtime_id,
                role: pl_core::AgentRoleId::new(role.to_string())?,
                session: pl_core::AgentSessionState::empty(session_id),
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
        self.send_message(agent_id, None, message, Vec::new()).await
    }

    pub(super) async fn wait_agent(
        &self,
        agent_id: AgentId,
        timeout: Duration,
    ) -> Result<AgentSummary> {
        let agent = self.agent(agent_id).await?;
        let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
        self.framework_handle()?
            .wait_timeout(runtime_agent_id, timeout)
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        let summary = agent.summary.read().await.clone();
        Ok(summary)
    }

    pub(super) async fn wait_agent_until_complete_with_cancel(
        &self,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> Result<pl_core::AgentWaitResult> {
        let agent = self.agent(agent_id).await?;
        let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
        let handle = self.framework_handle()?;
        tokio::select! {
            result = handle.wait(runtime_agent_id) => {
                result.map_err(|error| RuntimeError::InvalidInput(error.to_string()))
            }
            () = cancellation_token.cancelled() => Err(RuntimeError::TurnCancelled),
        }
    }

    pub(super) async fn agent_recent_messages(
        &self,
        agent_id: AgentId,
        limit: usize,
    ) -> Result<(Option<SessionId>, Vec<AgentMessage>)> {
        let agent = self.agent(agent_id).await?;
        let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
        let runtime = agent_host::load_runtime(&self.deps.store, &runtime_agent_id).await?;
        let sessions = agent_host::project_sessions(&runtime);
        let Some(session) = agent_host::selected_session(&sessions, None) else {
            return Ok((None, Vec::new()));
        };
        let start = session.messages.len().saturating_sub(limit);
        Ok((Some(session.summary.id), session.messages[start..].to_vec()))
    }
}
