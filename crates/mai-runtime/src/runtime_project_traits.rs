use super::*;

impl projects::service::ProjectLifecycleOps for Arc<AgentRuntime> {
    async fn save_project(&self, project: &ProjectSummary) -> Result<()> {
        self.deps.store.save_project(project).await?;
        Ok(())
    }

    async fn delete_project_from_store(&self, project_id: ProjectId) -> Result<()> {
        self.deps.store.delete_project(project_id).await?;
        Ok(())
    }

    async fn publish_project_event(&self, event: MaiProductEventKind) {
        self.events.publish(event).await;
    }

    fn start_project_review_loop_if_ready(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::start_project_review_loop_if_ready(self, project_id)
    }

    fn stop_project_review_loop(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = ()> + Send {
        AgentRuntime::stop_project_review_loop(self, project_id)
    }

    fn delete_agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::delete_agent(self.as_ref(), agent_id)
    }

    async fn cancel_project_agent(&self, agent_id: AgentId) -> Result<()> {
        if self.agent(agent_id).await.is_ok() {
            let _ = self.cancel_agent(agent_id).await;
        }
        Ok(())
    }

    async fn delete_project_sidecar(&self, project_id: ProjectId) -> Result<()> {
        AgentRuntime::delete_project_sidecar(self.as_ref(), project_id)
            .await
            .map(|_| ())
    }

    fn delete_project_workspace(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::delete_project_workspace(self.as_ref(), project_id)
    }

    async fn remove_project_from_memory(&self, project_id: ProjectId) {
        self.state.projects.write().await.remove(&project_id);
    }

    async fn remove_project_skill_lock(&self, project_id: ProjectId) {
        self.state
            .project_skill_locks
            .write()
            .await
            .remove(&project_id);
    }

    fn project_skill_cache_dir(&self, project_id: ProjectId) -> PathBuf {
        AgentRuntime::project_skill_cache_dir(self.as_ref(), project_id)
    }

    async fn send_project_agent_message(
        &self,
        agent_id: AgentId,
        request: SendMessageRequest,
    ) -> Result<TurnId> {
        AgentRuntime::send_message(
            self,
            agent_id,
            request.session_id,
            request.message,
            request.skill_mentions,
        )
        .await
    }
}

impl projects::service::ProjectCreateOps for Arc<AgentRuntime> {
    async fn list_github_installations(&self) -> Result<GithubInstallationsResponse> {
        AgentRuntime::list_github_installations(self.as_ref()).await
    }

    async fn upsert_github_app_relay_account(
        &self,
        installation_id: u64,
        account_login: &str,
    ) -> Result<String> {
        Ok(self
            .deps
            .store
            .upsert_github_app_relay_account(installation_id, account_login, "default", false)
            .await?
            .id)
    }

    async fn verified_repository(
        &self,
        account_id: &str,
        repository_full_name: &str,
    ) -> Result<github::VerifiedGithubRepository> {
        self.deps
            .git_accounts
            .verified_repository(account_id, repository_full_name)
            .await
    }

    async fn git_account_summary(&self, account_id: &str) -> Result<GitAccountSummary> {
        self.deps.git_accounts.summary(account_id).await
    }

    async fn planner_model(&self) -> Result<AgentModelPreference> {
        Ok(self
            .resolve_role_agent_model(AgentRole::Planner)
            .await?
            .preference)
    }

    async fn create_project_maintainer_agent(
        &self,
        request: projects::service::ProjectMaintainerAgentRequest,
    ) -> Result<AgentSummary> {
        let maintainer = agents::create_agent_record(
            self.as_ref(),
            CreateAgentRequest {
                name: Some(request.name),
                provider_id: Some(request.model.provider_id),
                model: Some(request.model.model),
                reasoning_effort: request.model.reasoning_effort,
                docker_image: request.docker_image,
                parent_id: None,
                system_prompt: Some(request.system_prompt),
            },
            agents::CreateAgentRecordContext {
                task_id: None,
                project_id: Some(request.project_id),
                role: Some(AgentRole::Planner),
            },
        )
        .await?;
        let summary = maintainer.summary.read().await.clone();
        self.register_framework_agent(summary.id).await?;
        Ok(summary)
    }

    async fn save_project(&self, project: &ProjectSummary) -> Result<()> {
        self.deps.store.save_project(project).await?;
        Ok(())
    }

    async fn insert_project(&self, project: ProjectSummary) {
        self.state
            .projects
            .write()
            .await
            .insert(project.id, Arc::new(ProjectRecord::new(project)));
    }

    async fn publish_project_event(&self, event: MaiProductEventKind) {
        self.events.publish(event).await;
    }

    async fn start_project_workspace(&self, project_id: ProjectId, maintainer_agent_id: AgentId) {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = runtime
                .start_project_workspace(project_id, maintainer_agent_id)
                .await
            {
                tracing::warn!(project_id = %project_id, "failed to finish project workspace setup: {err}");
            }
        });
    }
}
