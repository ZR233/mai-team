use super::*;

impl projects::service::ProjectReadOps for AgentRuntime {
    fn get_agent(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> impl std::future::Future<Output = Result<AgentDetail>> + Send {
        AgentRuntime::get_agent(self, agent_id, session_id)
    }

    async fn recent_review_runs(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<ProjectReviewRunSummary>> {
        Ok(self
            .list_project_review_runs(project_id, 0, PROJECT_REVIEW_RUN_LIST_LIMIT)
            .await?
            .runs)
    }
}

impl tasks::TaskReadOps for AgentRuntime {
    fn get_agent(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> impl std::future::Future<Output = Result<AgentDetail>> + Send {
        AgentRuntime::get_agent(self, agent_id, session_id)
    }
}

impl tasks::TaskUpdateOps for AgentRuntime {
    async fn save_task(&self, summary: &TaskSummary, plan: &TaskPlan) -> Result<()> {
        self.deps.store.save_task(summary, plan).await?;
        Ok(())
    }

    async fn publish_task_updated(&self, task: TaskSummary) {
        self.events
            .publish(MaiProductEventKind::TaskUpdated { task })
            .await;
    }
}

impl tasks::TaskPlanOps for AgentRuntime {
    async fn save_plan_history_entry(
        &self,
        task_id: TaskId,
        entry: &PlanHistoryEntry,
    ) -> Result<()> {
        self.deps
            .store
            .save_plan_history_entry(task_id, entry)
            .await?;
        Ok(())
    }

    async fn publish_plan_updated(&self, task_id: TaskId, plan: TaskPlan) {
        self.events
            .publish(MaiProductEventKind::PlanUpdated { task_id, plan })
            .await;
    }
}

impl tasks::TaskToolOps for AgentRuntime {
    async fn agent_summary(&self, agent_id: AgentId) -> Result<AgentSummary> {
        let agent = self.agent(agent_id).await?;
        Ok(agent.summary.read().await.clone())
    }

    async fn append_task_review(&self, review: &TaskReview) -> Result<()> {
        self.deps.store.append_task_review(review).await?;
        Ok(())
    }
}

impl tasks::TaskPlanningOps for Arc<AgentRuntime> {
    async fn send_agent_message(
        &self,
        agent_id: AgentId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        AgentRuntime::send_message(self, agent_id, None, message, skill_mentions).await
    }

    async fn spawn_task_workflow(&self, task_id: TaskId) {
        AgentRuntime::spawn_task_workflow(self, task_id);
    }
}

impl tasks::TaskLifecycleOps for Arc<AgentRuntime> {
    async fn cancel_agent_for_task(
        &self,
        agent_id: AgentId,
        current_turn: Option<TurnId>,
    ) -> Result<()> {
        if let Some(turn_id) = current_turn {
            AgentRuntime::cancel_agent_turn(self, agent_id, turn_id).await
        } else {
            Ok(())
        }
    }

    async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
        AgentRuntime::delete_agent(self, agent_id).await
    }

    async fn agent_current_turn(&self, agent_id: AgentId) -> Result<Option<TurnId>> {
        let record = self.agent(agent_id).await?;
        Ok(record.summary.read().await.state.active_turn())
    }

    async fn delete_task_from_store(&self, task_id: TaskId) -> Result<()> {
        self.deps.store.delete_task(task_id).await?;
        Ok(())
    }

    async fn publish_task_deleted(&self, task_id: TaskId) {
        self.events
            .publish(MaiProductEventKind::TaskDeleted { task_id })
            .await;
    }
}

impl tasks::TaskWorkflowOps for Arc<AgentRuntime> {
    async fn spawn_task_role_agent(
        &self,
        parent_agent_id: AgentId,
        role: AgentRole,
        name: Option<String>,
    ) -> Result<AgentSummary> {
        AgentRuntime::spawn_task_role_agent(self, parent_agent_id, role, name).await
    }

    async fn start_agent_turn(&self, agent_id: AgentId, message: String) -> Result<TurnId> {
        AgentRuntime::start_agent_turn(self, agent_id, message).await
    }

    async fn wait_agent(&self, agent_id: AgentId, timeout: Duration) -> Result<AgentSummary> {
        AgentRuntime::wait_agent(self.as_ref(), agent_id, timeout).await
    }
}

impl tasks::TaskArtifactOps for AgentRuntime {
    async fn agent_task_id(&self, agent_id: AgentId) -> Result<Option<TaskId>> {
        let agent = self.agent(agent_id).await?;
        Ok(agent.summary.read().await.task_id)
    }

    async fn agent_container_id(&self, agent_id: AgentId) -> Result<String> {
        self.container_id(agent_id).await
    }

    fn artifact_files_root(&self) -> PathBuf {
        self.artifact_files_root.clone()
    }

    async fn copy_artifact_from_container(
        &self,
        container_id: String,
        source_path: String,
        dest_path: PathBuf,
    ) -> Result<()> {
        self.deps
            .docker
            .copy_from_container_to_file(&container_id, &source_path, &dest_path)
            .await?;
        Ok(())
    }

    fn save_artifact_record(&self, info: &ArtifactInfo) -> Result<()> {
        self.deps.store.save_artifact(info)?;
        Ok(())
    }

    async fn publish_artifact_created(&self, info: ArtifactInfo) {
        self.events
            .publish(MaiProductEventKind::ArtifactCreated { artifact: info })
            .await;
    }
}

impl tasks::TaskCreateOps for Arc<AgentRuntime> {
    async fn planner_model(&self) -> Result<AgentModelPreference> {
        Ok(self
            .resolve_role_agent_model(AgentRole::Planner)
            .await?
            .preference)
    }

    async fn create_task_planner_agent(
        &self,
        request: tasks::CreateTaskPlannerAgentRequest,
    ) -> Result<AgentSummary> {
        self.create_agent_with_container_source(
            CreateAgentRequest {
                name: Some(format!("{} Planner", request.title)),
                provider_id: Some(request.model.provider_id),
                model: Some(request.model.model),
                reasoning_effort: request.model.reasoning_effort,
                docker_image: request.docker_image,
                parent_id: None,
                system_prompt: Some(
                    agents::task_role_system_prompt(AgentRole::Planner).to_string(),
                ),
            },
            agents::ContainerSource::FreshImage,
            Some(request.task_id),
            None,
            Some(AgentRole::Planner),
        )
        .await
    }

    async fn save_task(&self, summary: &TaskSummary, plan: &TaskPlan) -> Result<()> {
        self.deps.store.save_task(summary, plan).await?;
        Ok(())
    }

    async fn publish_task_event(&self, event: MaiProductEventKind) {
        self.events.publish(event).await;
    }

    fn send_task_message(
        &self,
        task_id: TaskId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> impl std::future::Future<Output = Result<TurnId>> + Send {
        AgentRuntime::send_task_message(self, task_id, message, skill_mentions)
    }

    async fn spawn_task_title_generation(&self, task_id: TaskId, message: String) {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            match runtime.generate_task_title(&message).await {
                Ok(new_title) => {
                    if let Err(err) = runtime.update_task_title(task_id, new_title).await {
                        tracing::warn!("failed to update task title: {err}");
                    }
                }
                Err(err) => {
                    tracing::warn!("failed to generate task title: {err}");
                }
            }
        });
    }
}

impl tasks::EnvironmentOps for Arc<AgentRuntime> {
    async fn environment_model(&self) -> Result<Option<AgentModelPreference>> {
        match self.resolve_role_agent_model(AgentRole::Planner).await {
            Ok(model) => Ok(Some(model.preference)),
            Err(RuntimeError::Store(mai_store::StoreError::InvalidConfig(_))) => Ok(None),
            Err(err) => Err(err),
        }
    }

    async fn create_environment_root_agent(
        &self,
        request: tasks::CreateEnvironmentRootAgentRequest,
    ) -> Result<AgentSummary> {
        let agent = match request.model {
            Some(model) => {
                agents::create_agent_record(
                    self.as_ref(),
                    CreateAgentRequest {
                        name: Some(request.name),
                        provider_id: Some(model.provider_id),
                        model: Some(model.model),
                        reasoning_effort: model.reasoning_effort,
                        docker_image: request.docker_image,
                        parent_id: None,
                        system_prompt: None,
                    },
                    agents::CreateAgentRecordContext {
                        task_id: Some(request.environment_id),
                        project_id: None,
                        role: Some(AgentRole::Planner),
                    },
                )
                .await?
            }
            None => {
                self.create_unconfigured_environment_root_agent(request)
                    .await?
            }
        };
        let summary = agent.summary.read().await.clone();
        self.register_framework_agent(summary.id).await?;
        self.start_environment_container_in_background(agent).await;
        Ok(summary)
    }

    fn get_agent(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> impl std::future::Future<Output = Result<AgentDetail>> + Send {
        AgentRuntime::get_agent(self.as_ref(), agent_id, session_id)
    }

    fn create_agent_session(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<AgentSessionSummary>> + Send {
        AgentRuntime::create_session(self.as_ref(), agent_id)
    }

    fn send_agent_message(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> impl std::future::Future<Output = Result<TurnId>> + Send {
        AgentRuntime::send_message(self, agent_id, Some(session_id), message, skill_mentions)
    }
}
