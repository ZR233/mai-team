use super::*;

impl projects::review::runs::ReviewRunSnapshotSource for AgentRuntime {
    async fn snapshot(
        &self,
        reviewer_agent_id: AgentId,
    ) -> projects::review::runs::ReviewRunSnapshot {
        let token_usage = match self.agent(reviewer_agent_id).await {
            Ok(agent) => agent.summary.read().await.token_usage.clone(),
            Err(_) => Default::default(),
        };
        let (session_id, messages) = self
            .agent_recent_messages(reviewer_agent_id, PROJECT_REVIEW_SNAPSHOT_MESSAGE_LIMIT)
            .await
            .unwrap_or_default();
        let events = match session_id {
            Some(session_id) => self
                .session_recent_events(session_id, PROJECT_REVIEW_SNAPSHOT_EVENT_LIMIT)
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
        projects::review::runs::ReviewRunSnapshot {
            token_usage,
            messages,
            events,
        }
    }
}

impl projects::review::state::ProjectReviewStateOps for AgentRuntime {
    fn project(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Result<Arc<ProjectRecord>>> + Send {
        AgentRuntime::project(self, project_id)
    }

    async fn save_project(&self, project: ProjectSummary) -> Result<()> {
        self.deps.store.save_project(&project).await?;
        Ok(())
    }

    async fn publish_project_updated(&self, project: ProjectSummary) {
        self.events
            .publish(MaiProductEventKind::ProjectUpdated { project })
            .await;
    }
}

impl projects::review::cleanup::ProjectReviewCleanupOps for Arc<AgentRuntime> {
    async fn prune_project_review_runs_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        Ok(self
            .deps
            .store
            .prune_project_review_runs_before(cutoff)
            .await?)
    }

    async fn prune_product_events_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        Ok(self.deps.store.prune_product_events_before(cutoff).await?)
    }

    async fn prune_product_events_to_limit(&self, limit: usize) -> Result<usize> {
        Ok(self.deps.store.prune_product_events_to_limit(limit).await?)
    }

    async fn prune_agent_logs_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        Ok(self.deps.store.prune_agent_logs_before(cutoff).await?)
    }

    async fn prune_tool_traces_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        Ok(self.deps.store.prune_tool_traces_before(cutoff).await?)
    }

    async fn retain_events_since(&self, cutoff: DateTime<Utc>) {
        self.events.retain_since(cutoff).await;
    }

    async fn list_projects(&self) -> Vec<ProjectSummary> {
        AgentRuntime::list_projects(self.as_ref()).await
    }
}

impl projects::review::target::ProjectReviewTargetOps for Arc<AgentRuntime> {
    async fn project_summary(&self, project_id: ProjectId) -> Result<ProjectSummary> {
        let project = AgentRuntime::project(self.as_ref(), project_id).await?;
        Ok(project.summary.read().await.clone())
    }

    async fn github_api_get_json(&self, project_id: ProjectId, path: String) -> Result<Value> {
        github::project_github_api_get_json(
            &self.deps.github_http,
            &self.github_api_base_url,
            self.project_git_token(project_id).await?,
            &path,
        )
        .await
    }
}

impl projects::review::reviewer::ProjectReviewerAgentOps for Arc<AgentRuntime> {
    async fn agent_summary(&self, agent_id: AgentId) -> Result<AgentSummary> {
        let agent = AgentRuntime::agent(self.as_ref(), agent_id).await?;
        Ok(agent.summary.read().await.clone())
    }

    async fn reviewer_model(&self) -> Result<AgentModelPreference> {
        Ok(self
            .resolve_role_agent_model(AgentRole::Reviewer)
            .await?
            .preference)
    }

    fn project_reviewer_agents(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Vec<AgentSummary>> + Send {
        AgentRuntime::project_auto_reviewer_agents(self.as_ref(), project_id)
    }

    async fn sync_project_repository_for_review(
        &self,
        project_id: ProjectId,
        target: projects::review::target::ResolvedProjectReviewTarget,
    ) -> Result<projects::workspace::ProjectRepositoryRevision> {
        AgentRuntime::sync_project_repository(
            self.as_ref(),
            project_id,
            projects::workspace::ProjectRepositorySyncTarget::Review(
                projects::workspace::ProjectRepositoryReviewTarget {
                    pr: target.pr,
                    head_sha: target.head_sha,
                },
            ),
        )
        .await
    }

    fn create_project_review_context(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
        target: projects::review::target::ResolvedProjectReviewTarget,
        project_revision: projects::workspace::ProjectRepositoryRevision,
    ) -> impl std::future::Future<
        Output = Result<Arc<projects::review::context::ProjectReviewContext>>,
    > + Send {
        AgentRuntime::prepare_project_review_context(
            self.as_ref(),
            project_id,
            run_id,
            target,
            project_revision,
        )
    }

    fn create_agent_with_container_source(
        &self,
        request: CreateAgentRequest,
        source: agents::ContainerSource,
        task_id: Option<TaskId>,
        project_id: Option<ProjectId>,
        role: Option<AgentRole>,
    ) -> impl std::future::Future<Output = Result<AgentSummary>> + Send {
        AgentRuntime::create_agent_with_container_source(
            self, request, source, task_id, project_id, role,
        )
    }

    async fn attach_project_review_context(
        &self,
        agent_id: AgentId,
        context: Arc<projects::review::context::ProjectReviewContext>,
    ) -> Result<()> {
        let agent = AgentRuntime::agent(self.as_ref(), agent_id).await?;
        let summary = agent.summary.read().await;
        if summary.role != Some(AgentRole::Reviewer) {
            return Err(RuntimeError::InvalidInput(format!(
                "project review context cannot be attached to non-reviewer agent `{agent_id}`"
            )));
        }
        drop(summary);
        *agent.review_context.write().await = Some(context);
        Ok(())
    }

    async fn delete_project_review_context(
        &self,
        project_id: ProjectId,
        context: Arc<projects::review::context::ProjectReviewContext>,
    ) -> Result<()> {
        AgentRuntime::cleanup_project_review_context(self.as_ref(), project_id, &context).await
    }

    fn delete_agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::delete_agent(self.as_ref(), agent_id)
    }

    fn start_agent_turn(
        &self,
        agent_id: AgentId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> impl std::future::Future<Output = Result<TurnId>> + Send {
        AgentRuntime::send_message(self, agent_id, None, message, skill_mentions)
    }

    async fn last_turn_response(&self, agent_id: AgentId) -> Result<Option<String>> {
        let agent = AgentRuntime::agent(self.as_ref(), agent_id).await?;
        let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
        let runtime = agent_host::load_runtime(&self.deps.store, &runtime_agent_id).await?;
        Ok(agent_host::last_assistant_response(&runtime))
    }
}

impl projects::review::selector::ProjectReviewSelectorOps for Arc<AgentRuntime> {
    fn enqueue_project_reviews(
        &self,
        project_id: ProjectId,
        signals: Vec<ProjectReviewSignalInput>,
    ) -> impl std::future::Future<Output = Result<ProjectReviewQueueSummary>> + Send {
        AgentRuntime::enqueue_project_review_signals(self, project_id, signals, false)
    }
}

impl projects::review::cycle::ProjectReviewCycleOps for Arc<AgentRuntime> {
    async fn set_project_review_state(
        &self,
        project_id: ProjectId,
        status: ProjectReviewStatus,
        update: ReviewStateUpdate,
    ) -> Result<ProjectSummary> {
        AgentRuntime::set_project_review_state(self.as_ref(), project_id, status, update).await
    }

    async fn save_project_review_run_status(&self, summary: ProjectReviewRunSummary) -> Result<()> {
        projects::review::runs::save_project_review_run_status(
            &self.deps.store,
            summary,
            Vec::new(),
            Vec::new(),
        )
        .await
    }

    async fn load_project_review_run(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
    ) -> Result<Option<ProjectReviewRunDetail>> {
        Ok(self
            .deps
            .store
            .load_project_review_run(project_id, run_id)
            .await?)
    }

    async fn update_project_review_run_turn(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
        reviewer_agent_id: AgentId,
        turn_id: TurnId,
    ) -> Result<()> {
        projects::review::runs::update_project_review_run_turn(
            &self.deps.store,
            project_id,
            run_id,
            reviewer_agent_id,
            turn_id,
        )
        .await
    }

    async fn finish_project_review_run(&self, request: FinishReviewRun) -> Result<()> {
        projects::review::runs::finish_project_review_run(&self.deps.store, self.as_ref(), request)
            .await
    }

    fn prepare_project_reviewer(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
        request: projects::review::target::ProjectReviewRequest,
    ) -> impl std::future::Future<
        Output = Result<projects::review::reviewer::PreparedProjectReviewer>,
    > + Send {
        projects::review::reviewer::prepare_project_reviewer(self, project_id, run_id, request)
    }

    fn project_reviewer_initial_message(
        &self,
        project_id: ProjectId,
        reviewer_id: AgentId,
        target: projects::review::target::ResolvedProjectReviewTarget,
        project_revision: projects::workspace::ProjectRepositoryRevision,
    ) -> impl std::future::Future<Output = Result<String>> + Send {
        projects::review::reviewer::project_reviewer_initial_message(
            self,
            project_id,
            reviewer_id,
            target,
            project_revision,
        )
    }

    fn start_reviewer_turn(
        &self,
        reviewer_id: AgentId,
        message: String,
    ) -> impl std::future::Future<Output = Result<TurnId>> + Send {
        projects::review::reviewer::start_reviewer_turn(self, reviewer_id, message)
    }

    fn wait_agent_until_complete_with_cancel(
        &self,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> impl std::future::Future<Output = Result<pl_core::AgentWaitResult>> + Send {
        AgentRuntime::wait_agent_until_complete_with_cancel(
            self.as_ref(),
            agent_id,
            cancellation_token,
        )
    }

    fn reviewer_final_response(
        &self,
        reviewer_id: AgentId,
    ) -> impl std::future::Future<Output = Result<String>> + Send {
        projects::review::reviewer::last_turn_response(self, reviewer_id)
    }

    async fn reviewer_target_is_stale(&self, reviewer_id: AgentId) -> Result<bool> {
        let agent = AgentRuntime::agent(self.as_ref(), reviewer_id).await?;
        Ok(agent
            .review_context
            .read()
            .await
            .as_ref()
            .is_some_and(|context| context.target_is_stale()))
    }

    fn delete_agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::delete_agent(self.as_ref(), agent_id)
    }
}

impl projects::review::worker::ProjectReviewWorkerOps for Arc<AgentRuntime> {
    fn project(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Result<Arc<ProjectRecord>>> + Send {
        AgentRuntime::project(self.as_ref(), project_id)
    }

    async fn project_ids(&self) -> Vec<ProjectId> {
        let projects = self.state.projects.read().await;
        projects.keys().copied().collect()
    }

    fn project_auto_reviewer_agents(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Vec<AgentSummary>> + Send {
        AgentRuntime::project_auto_reviewer_agents(self.as_ref(), project_id)
    }

    async fn load_project_review_runs(
        &self,
        project_id: ProjectId,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<ProjectReviewRunSummary>> {
        Ok(self
            .deps
            .store
            .load_project_review_runs(project_id, None, offset, limit)
            .await?)
    }

    async fn finish_project_review_run(&self, request: FinishReviewRun) -> Result<()> {
        projects::review::runs::finish_project_review_run(&self.deps.store, self.as_ref(), request)
            .await
    }

    async fn cancel_active_project_review_runs(
        &self,
        project_id: ProjectId,
        reviewer_agent_id: Option<AgentId>,
        run_list_limit: usize,
    ) -> Result<()> {
        projects::review::runs::cancel_active_project_review_runs(
            &self.deps.store,
            self.as_ref(),
            project_id,
            reviewer_agent_id,
            run_list_limit,
        )
        .await
    }

    async fn record_project_review_startup_failure(
        &self,
        project_id: ProjectId,
        error: String,
    ) -> Result<()> {
        projects::review::runs::record_project_review_startup_failure(
            &self.deps.store,
            project_id,
            error,
        )
        .await
    }

    async fn set_project_review_state(
        &self,
        project_id: ProjectId,
        status: ProjectReviewStatus,
        update: ReviewStateUpdate,
    ) -> Result<ProjectSummary> {
        AgentRuntime::set_project_review_state(self.as_ref(), project_id, status, update).await
    }

    fn ensure_project_repository_ready(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::ensure_project_repository_ready(self.as_ref(), project_id)
    }

    async fn project_git_provider(&self, project_id: ProjectId) -> Result<Option<GitProvider>> {
        let project = AgentRuntime::project(self.as_ref(), project_id).await?;
        let Some(account_id) = project.summary.read().await.git_account_id.clone() else {
            return Ok(None);
        };
        Ok(Some(
            self.deps.git_accounts.summary(&account_id).await?.provider,
        ))
    }

    fn run_project_review_selector(
        &self,
        project_id: ProjectId,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<
        Output = Result<projects::review::selector::ProjectReviewSelectorRunResult>,
    > + Send {
        projects::review::selector::run_project_review_selector(
            self,
            project_id,
            cancellation_token,
        )
    }

    fn select_project_review_pr(
        &self,
        project_id: ProjectId,
        pr: u64,
        head_sha_hint: Option<String>,
    ) -> impl std::future::Future<
        Output = Result<Option<projects::review::eligibility::SelectedProjectReviewPr>>,
    > + Send {
        projects::review::eligibility::select_project_review_pr(self, project_id, pr, head_sha_hint)
    }

    fn enqueue_project_review_signals(
        &self,
        project_id: ProjectId,
        signals: Vec<ProjectReviewSignalInput>,
    ) -> impl std::future::Future<Output = Result<ProjectReviewQueueSummary>> + Send {
        AgentRuntime::enqueue_project_review_signals(self, project_id, signals, false)
    }

    fn run_project_review_once(
        &self,
        project_id: ProjectId,
        cancellation_token: CancellationToken,
        request: projects::review::target::ProjectReviewRequest,
    ) -> impl std::future::Future<Output = Result<ProjectReviewCycleResult>> + Send {
        AgentRuntime::run_project_review_once(self, project_id, cancellation_token, request)
    }

    async fn agent_current_turn(&self, agent_id: AgentId) -> Result<Option<TurnId>> {
        let agent = AgentRuntime::agent(self.as_ref(), agent_id).await?;
        Ok(agent.summary.read().await.state.active_turn())
    }

    fn cancel_agent_turn(
        &self,
        agent_id: AgentId,
        turn_id: TurnId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::cancel_agent_turn(self, agent_id, turn_id)
    }

    fn delete_agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::delete_agent(self.as_ref(), agent_id)
    }
}
