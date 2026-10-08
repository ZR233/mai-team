use super::*;

impl projects::review::runs::ReviewRunSnapshotSource for AgentRuntime {
    async fn snapshot(
        &self,
        reviewer_agent_id: AgentId,
        turn_id: Option<&str>,
    ) -> Result<projects::review::runs::ReviewRunSnapshot> {
        // typed SessionHistory 读取已提交 effect；同时保证 reviewer Thread 已驻留，使下面的
        // canonical usage_summary 与刚完成的 Turn 属于同一 owner。
        let page = self
            .thread_turns(
                reviewer_agent_id.to_string(),
                None,
                REVIEW_SNAPSHOT_TURN_LIMIT,
            )
            .await?;
        let updated_at = now().timestamp();
        let usage = self
            .resident_thread(reviewer_agent_id)
            .map(|resident| {
                projects::review::runs::runtime_usage_snapshot(
                    &resident.handle.snapshot().usage_summary,
                    updated_at,
                )
            })
            .unwrap_or_default();
        let history = page.turns.into_iter().find(|history| {
            turn_id.is_none_or(|input_id| history.turn.input_id.as_deref() == Some(input_id))
        });
        Ok(projects::review::runs::ReviewRunSnapshot { usage, history })
    }
}

/// 一次 Run 终态快照最多回看的已提交 Thread effect 数。
const REVIEW_SNAPSHOT_TURN_LIMIT: usize = 200;

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
    async fn retention_config(&self) -> MaiRetentionConfig {
        self.mai_config.read().await.retention.clone()
    }

    async fn prune_retired_agent_sessions_before(
        &self,
        cutoff: DateTime<Utc>,
        batch_size: usize,
    ) -> Result<usize> {
        let ids = self
            .deps
            .store
            .expired_agent_sessions(cutoff, batch_size)
            .await?;
        for id in &ids {
            if self.resident_thread(*id).is_some() {
                return Err(RuntimeError::InvalidInput(format!(
                    "retired Agent `{id}` still has a resident Thread"
                )));
            }
            self.session_history.delete(*id).await.map_err(|error| {
                RuntimeError::InvalidInput(format!(
                    "could not delete PL session for retired Agent `{id}`: {error}"
                ))
            })?;
            self.deps
                .store
                .complete_agent_session_retirement(*id)
                .await?;
        }
        Ok(ids.len())
    }

    async fn prune_project_review_jobs_before(
        &self,
        cutoff: DateTime<Utc>,
        batch_size: usize,
    ) -> Result<usize> {
        Ok(self
            .deps
            .store
            .prune_project_review_jobs_before_batch(cutoff, Utc::now(), batch_size)
            .await?)
    }

    async fn prune_orphan_project_review_runs_before(
        &self,
        cutoff: DateTime<Utc>,
        batch_size: usize,
    ) -> Result<usize> {
        Ok(self
            .deps
            .store
            .prune_orphan_project_review_runs_before_batch(cutoff, batch_size)
            .await?)
    }

    async fn prune_product_events_before(
        &self,
        cutoff: DateTime<Utc>,
        batch_size: usize,
    ) -> Result<usize> {
        Ok(self
            .deps
            .store
            .prune_product_events_before_batch(cutoff, batch_size)
            .await?)
    }

    async fn prune_product_events_to_limit(
        &self,
        limit: usize,
        batch_size: usize,
    ) -> Result<usize> {
        Ok(self
            .deps
            .store
            .prune_product_events_to_limit_batch(limit, batch_size)
            .await?)
    }

    async fn prune_agent_logs_before(
        &self,
        cutoff: DateTime<Utc>,
        batch_size: usize,
    ) -> Result<usize> {
        Ok(self
            .deps
            .store
            .prune_agent_logs_before_batch(cutoff, batch_size)
            .await?)
    }

    async fn prune_tool_traces_before(
        &self,
        cutoff: DateTime<Utc>,
        batch_size: usize,
    ) -> Result<usize> {
        Ok(self
            .deps
            .store
            .prune_tool_traces_before_batch(cutoff, batch_size)
            .await?)
    }

    async fn cleanup_tool_output_namespaces(
        &self,
        cutoff: std::time::SystemTime,
        batch_size: usize,
    ) -> Result<usize> {
        AgentRuntime::cleanup_tool_output_namespaces(self.as_ref(), cutoff, batch_size).await
    }

    async fn retain_events_since(&self, cutoff: DateTime<Utc>) {
        self.events.retain_since(cutoff).await;
    }

    async fn reconcile_project_volumes(&self) -> Result<()> {
        let projects = AgentRuntime::list_projects(self.as_ref()).await;
        let agents = AgentRuntime::list_agents(self.as_ref()).await;
        let _ = projects::workspace::docker_reconcile::reconcile_project_volumes(
            &self.deps.docker,
            &projects,
            &agents,
        )
        .await?;
        Ok(())
    }

    async fn claim_due_project_review_cleanup_task(
        &self,
        owner: String,
        now: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> Result<Option<mai_store::ProjectReviewCleanupTask>> {
        Ok(self
            .deps
            .store
            .claim_due_project_review_cleanup_task(owner, now, lease_expires_at)
            .await?)
    }

    async fn execute_project_review_cleanup_task(
        &self,
        task: mai_store::ProjectReviewCleanupTask,
    ) -> Result<()> {
        match task.resource_kind {
            mai_store::ProjectReviewCleanupResourceKind::ReviewerAgent => {
                let agent_id = Uuid::parse_str(&task.resource_id).map_err(|error| {
                    RuntimeError::InvalidInput(format!(
                        "invalid cleanup reviewer agent id {}: {error}",
                        task.resource_id
                    ))
                })?;
                match AgentRuntime::delete_agent(self.as_ref(), agent_id).await {
                    Ok(()) | Err(RuntimeError::AgentNotFound(_)) => Ok(()),
                    Err(error) => Err(error),
                }
            }
            mai_store::ProjectReviewCleanupResourceKind::ReviewContext => {
                let run_id = Uuid::parse_str(&task.resource_id).map_err(|error| {
                    RuntimeError::InvalidInput(format!(
                        "invalid cleanup review run id {}: {error}",
                        task.resource_id
                    ))
                })?;
                self.cleanup_project_review_context_by_run_id(task.project_id, run_id)
                    .await
            }
            mai_store::ProjectReviewCleanupResourceKind::ToolOutputNamespace => {
                let agent_id = Uuid::parse_str(&task.resource_id).map_err(|error| {
                    RuntimeError::InvalidInput(format!(
                        "invalid cleanup tool-output agent id {}: {error}",
                        task.resource_id
                    ))
                })?;
                self.cleanup_agent_tool_output_namespace(agent_id).await
            }
        }
    }

    async fn complete_project_review_cleanup_task(
        &self,
        task_id: String,
        owner: String,
        finished_at: DateTime<Utc>,
    ) -> Result<bool> {
        Ok(self
            .deps
            .store
            .complete_project_review_cleanup_task(task_id, owner, finished_at)
            .await?)
    }

    async fn retry_project_review_cleanup_task(
        &self,
        task_id: String,
        owner: String,
        next_attempt_at: DateTime<Utc>,
        error: String,
    ) -> Result<bool> {
        Ok(self
            .deps
            .store
            .retry_project_review_cleanup_task(task_id, owner, next_attempt_at, error)
            .await?)
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

    async fn agent_system_prompt(&self, agent_id: AgentId) -> Result<Option<String>> {
        let agent = AgentRuntime::agent(self.as_ref(), agent_id).await?;
        Ok(agent.system_prompt.clone())
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

    async fn attached_project_review_context(
        &self,
        agent_id: AgentId,
    ) -> Result<Option<Arc<projects::review::context::ProjectReviewContext>>> {
        let agent = AgentRuntime::agent(self.as_ref(), agent_id).await?;
        Ok(agent.review_context.read().await.clone())
    }

    async fn ensure_project_reviewer_thread(&self, agent_id: AgentId) -> Result<()> {
        self.ensure_thread(agent_id).await.map(|_| ())
    }

    async fn ensure_project_reviewer_container(
        &self,
        agent_id: AgentId,
        run_id: Uuid,
        target: projects::review::target::ResolvedProjectReviewTarget,
        project_revision: projects::workspace::ProjectRepositoryRevision,
        repository_view: projects::review::context::ProjectRepositoryView,
    ) -> Result<()> {
        let agent = AgentRuntime::agent(self.as_ref(), agent_id).await?;
        let project_id = agent.summary.read().await.project_id.ok_or_else(|| {
            RuntimeError::InvalidInput("project reviewer is not attached to a project".to_string())
        })?;
        let source = self
            .agent_container_source_for_project(
                agent_id,
                Some(project_id),
                agents::ContainerSource::ProjectReviewWorkspace {
                    run_id,
                    target: projects::workspace::ProjectRepositoryReviewTarget {
                        pr: target.pr,
                        head_sha: target.head_sha,
                    },
                    revision: project_revision,
                    repository_view,
                },
            )
            .await?;
        agents::ensure_agent_container_with_source(self.as_ref(), &agent, &source)
            .await
            .map(|_| ())
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
        AgentRuntime::send_message(self, agent_id, message, skill_mentions)
    }

    async fn last_turn_response(&self, agent_id: AgentId) -> Result<Option<String>> {
        // 最终回复是 typed history 里最后一条 Final 文本 Item，不在 store 或旧 AgentSnapshot 里。
        let page = self
            .thread_turns(
                agent_id.to_string(),
                None,
                REVIEWER_FINAL_RESPONSE_TURN_LIMIT,
            )
            .await?;
        Ok(page
            .turns
            .iter()
            .find_map(|history| last_final_text(&history.items)))
    }
}

/// 读取 reviewer 最终回复最多回看的已提交 Thread effect 数。
const REVIEWER_FINAL_RESPONSE_TURN_LIMIT: usize = 64;

/// 返回一条已提交 Turn history 中最后一条 Final 文本。
fn last_final_text(items: &[mai_protocol::ThreadItem]) -> Option<String> {
    items.iter().rev().find_map(|item| {
        let text = item.text()?;
        (text.channel() == mai_protocol::ThreadTextChannel::Final).then(|| text.text().to_string())
    })
}

impl projects::review::selector::ProjectReviewSelectorOps for Arc<AgentRuntime> {
    fn admit_project_review_discovery(
        &self,
        project_id: ProjectId,
        input: projects::review::selector::ProjectReviewDiscoveryAdmissionInput,
    ) -> impl std::future::Future<
        Output = Result<projects::review::selector::ProjectReviewDiscoveryAdmissionResult>,
    > + Send {
        AgentRuntime::admit_project_review_discovery(self, project_id, input)
    }
}

impl projects::review::discovery::ProjectReviewDiscoveryOps for Arc<AgentRuntime> {
    async fn project_review_discovery_project_ids(&self) -> Vec<ProjectId> {
        let projects = self.state.projects.read().await;
        projects.keys().copied().collect()
    }

    async fn project_review_discovery_enabled(&self, project_id: ProjectId) -> Result<bool> {
        let project = AgentRuntime::project(self.as_ref(), project_id).await?;
        let summary = project.summary.read().await.clone();
        if !summary.auto_review_enabled
            || summary.status != ProjectStatus::Ready
            || summary.clone_status != ProjectCloneStatus::Ready
        {
            return Ok(false);
        }
        let Some(account_id) = summary.git_account_id else {
            return Ok(false);
        };
        let provider = self.deps.git_accounts.summary(&account_id).await?.provider;
        Ok(matches!(
            provider,
            GitProvider::Github | GitProvider::GithubAppRelay
        ))
    }

    fn run_project_review_discovery(
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

    async fn project_review_discovery_snapshot(
        &self,
        project_id: ProjectId,
    ) -> Result<ProjectReviewDiscoverySnapshot> {
        let project = self.project(project_id).await?;
        Ok(project.review_discovery.read().await.clone())
    }

    async fn update_project_review_discovery_snapshot(
        &self,
        project_id: ProjectId,
        snapshot: ProjectReviewDiscoverySnapshot,
    ) -> Result<()> {
        let project = self.project(project_id).await?;
        *project.review_discovery.write().await = snapshot;
        self.events
            .publish(MaiProductEventKind::ProjectReviewDiscoveryUpdated { project_id })
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mai_protocol::{ThreadItem, ThreadItemState, ThreadTextChannel};
    use pl_protocol::{ThreadContentLifecycle, ThreadTextItem};
    use pretty_assertions::assert_eq;

    use super::last_final_text;

    #[test]
    fn final_response_uses_last_final_text_channel() {
        let items = vec![
            text_item("user", ThreadTextChannel::User, "question"),
            text_item("final-1", ThreadTextChannel::Final, "first"),
            text_item("commentary", ThreadTextChannel::Commentary, "working"),
            text_item("final-2", ThreadTextChannel::Final, "second"),
        ];

        assert_eq!(Some("second".to_string()), last_final_text(&items));
    }

    fn text_item(id: &str, channel: ThreadTextChannel, text: &str) -> ThreadItem {
        ThreadItem::new(
            id.to_string(),
            "reviewer".to_string(),
            "turn".to_string(),
            0,
            0,
            0,
            0,
            ThreadItemState::Text(ThreadTextItem::new(
                channel,
                text.to_string(),
                Vec::new(),
                ThreadContentLifecycle::completed(0),
            )),
        )
    }
}

impl projects::review::cycle::ProjectReviewCycleOps for Arc<AgentRuntime> {
    #[cfg(test)]
    async fn set_project_review_state(
        &self,
        project_id: ProjectId,
        status: ProjectReviewStatus,
        update: ReviewStateUpdate,
    ) -> Result<ProjectSummary> {
        AgentRuntime::set_project_review_state(self.as_ref(), project_id, status, update).await
    }

    async fn save_project_review_run_status(&self, summary: ProjectReviewRunSummary) -> Result<()> {
        projects::review::runs::save_project_review_run_status(&self.deps.store, summary, None)
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

    #[cfg(test)]
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

    fn wait_agent_turn(
        &self,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> impl std::future::Future<Output = Result<crate::runtime_agent_api::ThreadWaitOutcome>> + Send
    {
        AgentRuntime::wait_agent_turn(self.as_ref(), agent_id, cancellation_token)
    }

    async fn reviewer_progress(
        &self,
        reviewer_id: AgentId,
    ) -> Result<projects::review::cycle::ReviewerProgress> {
        let resident = self.ensure_thread(reviewer_id).await?;
        let state = resident.handle.snapshot();
        Ok(projects::review::cycle::ReviewerProgress {
            revision: state.commit_sequence,
            inactivity_timeout: reviewer_inactivity_timeout(&state),
        })
    }

    fn cancel_reviewer_turn(
        &self,
        reviewer_id: AgentId,
        turn_id: TurnId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::cancel_agent_turn(self, reviewer_id, turn_id)
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

    #[cfg(test)]
    fn delete_agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::delete_agent(self.as_ref(), agent_id)
    }
}

impl projects::review::job_attempt::ProjectReviewJobAttemptOps for Arc<AgentRuntime> {
    async fn load_project_review_job(
        &self,
        project_id: ProjectId,
        job_id: Uuid,
    ) -> Result<Option<ProjectReviewJobSummary>> {
        Ok(self
            .deps
            .store
            .load_project_review_job(project_id, job_id)
            .await?)
    }

    async fn save_claimed_project_review_job(
        &self,
        job: ProjectReviewJobSummary,
        owner: String,
    ) -> Result<bool> {
        Ok(self
            .deps
            .store
            .save_claimed_project_review_job(job, owner)
            .await?)
    }

    async fn begin_claimed_project_review_attempt(
        &self,
        job_id: Uuid,
        owner: String,
        run_id: Uuid,
        started_at: DateTime<Utc>,
    ) -> Result<ProjectReviewJobSummary> {
        Ok(self
            .deps
            .store
            .begin_claimed_project_review_attempt(job_id, owner, run_id, started_at)
            .await?)
    }

    fn resume_project_reviewer(
        &self,
        job: ProjectReviewJobSummary,
        reviewer_id: AgentId,
    ) -> impl std::future::Future<
        Output = Result<projects::review::reviewer::PreparedProjectReviewer>,
    > + Send {
        projects::review::reviewer::resume_project_reviewer(
            self,
            job.project_id,
            job.id,
            reviewer_id,
            projects::review::target::ProjectReviewRequest {
                pr: job.pr,
                head_sha_hint: Some(job.head_sha),
            },
        )
    }

    async fn prepare_project_reviewer_image(
        &self,
        project_id: ProjectId,
    ) -> Result<projects::review::reviewer::PreparedProjectReviewerImage> {
        const IMAGE_REFRESH_TIMEOUT: Duration = Duration::from_secs(15 * 60);
        let project = AgentRuntime::project(self.as_ref(), project_id).await?;
        let project = project.summary.read().await.clone();
        let maintainer = AgentRuntime::agent(self.as_ref(), project.maintainer_agent_id).await?;
        let maintainer = maintainer.summary.read().await.clone();
        let outcome = self
            .deps
            .docker
            .refresh_floating_latest_image(&maintainer.docker_image, IMAGE_REFRESH_TIMEOUT)
            .await?;
        let (docker_image, environment_warning) = match outcome {
            mai_docker::ImageRefreshOutcome::NotRequired { image } => {
                tracing::debug!(project_id = %project_id, image, "reviewer image refresh not required");
                (image, None)
            }
            mai_docker::ImageRefreshOutcome::UpToDate {
                image,
                image_id,
                elapsed,
            } => {
                tracing::info!(
                    project_id = %project_id,
                    image,
                    image_id,
                    elapsed_ms = elapsed.as_millis(),
                    "reviewer latest image is up to date"
                );
                (image, None)
            }
            mai_docker::ImageRefreshOutcome::Updated {
                image,
                previous_image_id,
                image_id,
                elapsed,
            } => {
                tracing::info!(
                    project_id = %project_id,
                    image,
                    previous_image_id,
                    image_id,
                    elapsed_ms = elapsed.as_millis(),
                    "reviewer latest image was updated"
                );
                (image, None)
            }
            mai_docker::ImageRefreshOutcome::CachedFallback {
                image,
                image_id,
                elapsed,
                error,
            } => {
                let message = sanitize_review_environment_warning(&error);
                tracing::warn!(
                    project_id = %project_id,
                    image,
                    image_id,
                    elapsed_ms = elapsed.as_millis(),
                    error = message,
                    "reviewer latest image refresh failed; using cached image"
                );
                let cached_image_id = image_id.clone();
                (
                    cached_image_id.clone(),
                    Some(ProjectReviewEnvironmentWarning {
                        code: "latest_image_refresh_failed".to_string(),
                        image,
                        cached_image_id,
                        message,
                        observed_at: now(),
                    }),
                )
            }
        };
        Ok(projects::review::reviewer::PreparedProjectReviewerImage {
            docker_image,
            environment_warning,
        })
    }

    fn prepare_project_reviewer_with_image(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
        request: projects::review::target::ProjectReviewRequest,
        docker_image: String,
    ) -> impl std::future::Future<
        Output = Result<projects::review::reviewer::PreparedProjectReviewer>,
    > + Send {
        projects::review::reviewer::prepare_project_reviewer_with_image(
            self,
            project_id,
            run_id,
            request,
            docker_image,
        )
    }

    async fn cleanup_timed_out_review_preparation(&self, project_id: ProjectId) -> Result<()> {
        for reviewer in self.project_auto_reviewer_agents(project_id).await {
            AgentRuntime::delete_agent(self.as_ref(), reviewer.id).await?;
        }
        Ok(())
    }

    async fn refresh_project_review_job_projection(&self, job: ProjectReviewJobSummary) {
        projects::review::job_worker::refresh_project_review_job_projection(
            self,
            job.project_id,
            &job,
            None,
            None,
        )
        .await;
    }
}

fn sanitize_review_environment_warning(error: &str) -> String {
    const MAX_WARNING_CHARS: usize = 512;
    let mut redact_next = false;
    let compact = error
        .split_whitespace()
        .map(|word| {
            if redact_next {
                redact_next = false;
                return "[redacted]".to_string();
            }
            let lowercase = word.to_ascii_lowercase();
            if lowercase == "bearer" || lowercase == "authorization:" {
                redact_next = true;
                return word.to_string();
            }
            if lowercase.contains("token=")
                || lowercase.contains("password=")
                || lowercase.starts_with("ghp_")
                || lowercase.starts_with("github_pat_")
                || lowercase.starts_with("gho_")
                || lowercase.starts_with("ghs_")
                || lowercase.starts_with("glpat-")
            {
                return "[redacted]".to_string();
            }
            redact_url_userinfo(word)
        })
        .collect::<Vec<_>>()
        .join(" ");
    compact.chars().take(MAX_WARNING_CHARS).collect()
}

fn redact_url_userinfo(word: &str) -> String {
    let Some(scheme_end) = word.find("://").map(|index| index + 3) else {
        return word.to_string();
    };
    let Some(relative_at) = word[scheme_end..].find('@') else {
        return word.to_string();
    };
    let at = scheme_end + relative_at;
    format!("{}[redacted]{}", &word[..scheme_end], &word[at..])
}

/// reviewer 的运行看门狗在没有任何 canonical 进展时允许的最长停顿。
///
/// 默认十分钟以上的停顿都算无进展；当 canonical Thread 里仍有正在运行的 exec 任务时，按该任务
/// 声明的 `timeoutSeconds` 加固定宽限延长，避免把正常的长工具执行误判为卡死。所有事实都来自
/// typed core 快照，不从旧 AgentSnapshot 或工具文本反推。
fn reviewer_inactivity_timeout(state: &pl_core::thread::ThreadSnapshot) -> std::time::Duration {
    const RUNNING_INACTIVITY_SECS: u64 = 10 * 60;
    const TOOL_TIMEOUT_GRACE_SECS: u64 = 60;
    let mut timeout = std::time::Duration::from_secs(RUNNING_INACTIVITY_SECS);
    let declared_timeout = state
        .tasks
        .values()
        .filter(|task| task.status == pl_core::thread::task::TaskStatus::Running)
        .filter(|task| task.tool_id == pl_tool::exec::TOOL_EXEC)
        .find_map(|task| running_tool_timeout_seconds(state, &task.call_id));
    if let Some(declared_timeout) = declared_timeout {
        timeout = timeout.max(std::time::Duration::from_secs(
            declared_timeout.saturating_add(TOOL_TIMEOUT_GRACE_SECS),
        ));
    }
    timeout
}

/// 在仍驻留的 attempt 输出里找到指定 call 的声明超时秒数。
fn running_tool_timeout_seconds(
    _state: &pl_core::thread::ThreadSnapshot,
    _call_id: &str,
) -> Option<u64> {
    // pl-core 的驻留快照不再保留已提交模型输出；工具超时参数由执行任务的产品配置负责。
    None
}

impl projects::review::ci_watch::ProjectReviewCiWatchOps for Arc<AgentRuntime> {
    async fn load_due_project_review_ci_watches(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<mai_store::ProjectReviewCiWatch>> {
        Ok(self
            .deps
            .store
            .load_due_project_review_ci_watches(now, limit)
            .await?)
    }

    fn evaluate_project_review_pr(
        &self,
        project_id: ProjectId,
        pr: u64,
        head_sha_hint: Option<String>,
    ) -> impl std::future::Future<
        Output = Result<projects::review::eligibility::EvaluatedProjectReviewPr>,
    > + Send {
        projects::review::eligibility::evaluate_project_review_pr(
            self,
            project_id,
            pr,
            head_sha_hint,
        )
    }

    fn enqueue_project_review_ci_watch(
        &self,
        watch: mai_store::ProjectReviewCiWatch,
        head_sha: String,
    ) -> impl std::future::Future<
        Output = Result<projects::review::ci_watch::ProjectReviewCiWatchAdmission>,
    > + Send {
        AgentRuntime::enqueue_project_review_ci_watch(self, watch, head_sha)
    }

    async fn replace_project_review_ci_watch_head(
        &self,
        watch: mai_store::ProjectReviewCiWatch,
        head_sha: String,
        next_check_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<bool> {
        Ok(self
            .deps
            .store
            .replace_project_review_ci_watch_head(
                watch.project_id,
                watch.pr,
                watch.head_sha,
                head_sha,
                next_check_at,
                updated_at,
            )
            .await?)
    }

    async fn reschedule_project_review_ci_watch(
        &self,
        watch: mai_store::ProjectReviewCiWatch,
        next_check_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<bool> {
        Ok(self
            .deps
            .store
            .reschedule_project_review_ci_watch(
                watch.project_id,
                watch.pr,
                watch.head_sha,
                next_check_at,
                updated_at,
            )
            .await?)
    }

    async fn delete_project_review_ci_watch(
        &self,
        watch: mai_store::ProjectReviewCiWatch,
    ) -> Result<bool> {
        Ok(self
            .deps
            .store
            .delete_project_review_ci_watch(watch.project_id, watch.pr, watch.head_sha)
            .await?)
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

    async fn finish_expired_project_review_run(&self, request: FinishReviewRun) -> Result<()> {
        projects::review::runs::finish_expired_project_review_run(
            &self.deps.store,
            self.as_ref(),
            request,
        )
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

    #[cfg(test)]
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

    #[cfg(test)]
    fn enqueue_project_review_signals(
        &self,
        project_id: ProjectId,
        signals: Vec<ProjectReviewSignalInput>,
    ) -> impl std::future::Future<Output = Result<ProjectReviewQueueSummary>> + Send {
        AgentRuntime::enqueue_project_review_signals(self, project_id, signals, false)
    }

    #[cfg(test)]
    fn run_project_review_once(
        &self,
        project_id: ProjectId,
        cancellation_token: CancellationToken,
        request: projects::review::target::ProjectReviewRequest,
    ) -> impl std::future::Future<Output = Result<ProjectReviewCycleResult>> + Send {
        AgentRuntime::run_project_review_once(self, project_id, cancellation_token, request)
    }

    async fn project_has_active_review_jobs(&self, project_id: ProjectId) -> Result<bool> {
        Ok(self
            .deps
            .store
            .project_has_active_review_jobs(project_id)
            .await?)
    }

    async fn evaluate_project_review_pr(
        &self,
        project_id: ProjectId,
        pr: u64,
        head_sha_hint: Option<String>,
    ) -> Result<projects::review::eligibility::EvaluatedProjectReviewPr> {
        projects::review::eligibility::evaluate_project_review_pr(
            self,
            project_id,
            pr,
            head_sha_hint,
        )
        .await
    }

    async fn enqueue_project_review_replacement(
        &self,
        job: ProjectReviewJobSummary,
        head_sha: String,
    ) -> Result<ProjectReviewQueueSummary> {
        AgentRuntime::enqueue_project_review_replacement(self, job, head_sha).await
    }

    async fn claim_due_project_review_job(
        &self,
        project_id: ProjectId,
        owner: String,
        now: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> Result<Option<ProjectReviewJobSummary>> {
        Ok(self
            .deps
            .store
            .claim_due_project_review_job(project_id, owner, now, lease_expires_at)
            .await?)
    }

    async fn load_project_review_job(
        &self,
        project_id: ProjectId,
        job_id: Uuid,
    ) -> Result<Option<ProjectReviewJobSummary>> {
        Ok(self
            .deps
            .store
            .load_project_review_job(project_id, job_id)
            .await?)
    }

    async fn load_active_project_review_job(
        &self,
        project_id: ProjectId,
    ) -> Result<Option<ProjectReviewJobSummary>> {
        Ok(self
            .deps
            .store
            .load_active_project_review_job(project_id)
            .await?)
    }

    async fn load_reviewer_owned_active_project_review_job(
        &self,
        project_id: ProjectId,
    ) -> Result<Option<ProjectReviewJobSummary>> {
        Ok(self
            .deps
            .store
            .load_reviewer_owned_active_project_review_job(project_id)
            .await?)
    }

    async fn save_claimed_project_review_job(
        &self,
        job: ProjectReviewJobSummary,
        owner: String,
    ) -> Result<bool> {
        Ok(self
            .deps
            .store
            .save_claimed_project_review_job(job, owner)
            .await?)
    }

    async fn skip_claimed_project_review_job_for_ci_pending(
        &self,
        job_id: Uuid,
        owner: String,
        expected_delivery_id: Option<String>,
        updated_at: DateTime<Utc>,
        next_check_at: DateTime<Utc>,
    ) -> Result<mai_store::ProjectReviewCiPendingSkipResult> {
        Ok(self
            .deps
            .store
            .skip_claimed_project_review_job_for_ci_pending(
                job_id,
                owner,
                expected_delivery_id,
                updated_at,
                next_check_at,
            )
            .await?)
    }

    async fn heartbeat_project_review_job(
        &self,
        job_id: Uuid,
        owner: String,
        updated_at: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> Result<bool> {
        Ok(self
            .deps
            .store
            .heartbeat_project_review_job(job_id, owner, updated_at, lease_expires_at)
            .await?)
    }

    async fn recover_expired_project_review_jobs(&self, now: DateTime<Utc>) -> Result<usize> {
        Ok(self
            .deps
            .store
            .recover_expired_project_review_jobs(now)
            .await?)
    }

    async fn archive_expired_project_review_runs(&self, now: DateTime<Utc>) -> Result<usize> {
        projects::review::runs::archive_expired_project_review_runs(
            &self.deps.store,
            self.as_ref(),
            now,
        )
        .await
    }

    async fn release_expired_archived_terminal_project_review_ownership(
        &self,
        now: DateTime<Utc>,
    ) -> Result<usize> {
        Ok(self
            .deps
            .store
            .release_expired_archived_terminal_project_review_ownership(now)
            .await?)
    }

    async fn recover_expired_terminal_project_review_runs(
        &self,
        now: DateTime<Utc>,
    ) -> Result<usize> {
        projects::review::runs::recover_expired_terminal_project_review_runs(
            &self.deps.store,
            self.as_ref(),
            now,
        )
        .await
    }

    async fn finish_owned_terminal_project_review_run(
        &self,
        job: ProjectReviewJobSummary,
        owner: String,
        now: DateTime<Utc>,
    ) -> Result<()> {
        projects::review::runs::finish_owned_terminal_project_review_run(
            &self.deps.store,
            self.as_ref(),
            job,
            owner,
            now,
        )
        .await
    }

    async fn cancel_active_project_review_jobs(
        &self,
        project_id: ProjectId,
        now: DateTime<Utc>,
    ) -> Result<usize> {
        Ok(self
            .deps
            .store
            .cancel_active_project_review_jobs(project_id, now)
            .await?)
    }

    fn run_project_review_job_attempt(
        &self,
        job: ProjectReviewJobSummary,
        owner: String,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = Result<ProjectReviewCycleResult>> + Send {
        AgentRuntime::run_project_review_job_attempt(self, job, owner, cancellation_token)
    }

    async fn reconcile_project_review_job(
        &self,
        job: ProjectReviewJobSummary,
    ) -> Result<Option<ProjectReviewSubmissionReceipt>> {
        let intent = job.submission_intent.as_ref().ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "review job {} has no submission intent to reconcile",
                job.id
            ))
        })?;
        let project = AgentRuntime::project(self.as_ref(), job.project_id).await?;
        let project_summary = project.summary.read().await.clone();
        let token = self
            .project_git_token(job.project_id)
            .await?
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "project git account token is not configured".to_string(),
                )
            })?;
        self.reconcile_project_review_submission(&token, &project_summary, intent)
            .await
    }

    async fn agent_current_turn(&self, agent_id: AgentId) -> Result<Option<TurnId>> {
        let agent = AgentRuntime::agent(self.as_ref(), agent_id).await?;
        Ok(agent.summary.read().await.active_turn())
    }

    fn cancel_agent_turn(
        &self,
        agent_id: AgentId,
        turn_id: TurnId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::cancel_agent_turn(self, agent_id, turn_id)
    }

    async fn find_project_review_job_reviewer(
        &self,
        job: ProjectReviewJobSummary,
    ) -> Result<Option<AgentId>> {
        if job.reviewer_agent_id.is_some() {
            return Ok(job.reviewer_agent_id);
        }
        let request = projects::review::target::ProjectReviewRequest {
            pr: job.pr,
            head_sha_hint: Some(job.head_sha.clone()),
        };
        for reviewer in self.project_auto_reviewer_agents(job.project_id).await {
            if projects::review::reviewer::reviewer_belongs_to_job(
                self,
                reviewer.id,
                job.id,
                &request,
            )
            .await?
            {
                return Ok(Some(reviewer.id));
            }
        }
        Ok(None)
    }

    fn delete_agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        AgentRuntime::delete_agent(self.as_ref(), agent_id)
    }
}

#[cfg(test)]
mod image_warning_tests {
    use super::sanitize_review_environment_warning;

    #[test]
    fn image_warning_diagnostic_is_bounded_and_redacts_credentials() {
        let message = sanitize_review_environment_warning(
            "pull https://user:secret@registry.example failed Authorization: Bearer-secret token=abc ghp_secret",
        );

        assert_eq!(
            "pull https://[redacted]@registry.example failed Authorization: [redacted] [redacted] [redacted]",
            message
        );
    }
}
