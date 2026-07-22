use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectReviewEnqueueAdmission {
    RequireAutoReviewEnabled,
    ManualRequest,
}

impl AgentRuntime {
    pub async fn list_tasks(&self) -> Vec<TaskSummary> {
        tasks::list_tasks(&self.state).await
    }

    pub async fn list_environments(&self) -> Vec<EnvironmentSummary> {
        tasks::list_environments(&self.state, self).await
    }

    pub async fn list_projects(&self) -> Vec<ProjectSummary> {
        projects::service::list_projects(&self.state).await
    }

    pub fn runtime_defaults(&self) -> RuntimeDefaultsResponse {
        RuntimeDefaultsResponse {
            default_docker_image: self.deps.docker.image().to_string(),
        }
    }

    pub async fn ensure_default_task(self: &Arc<Self>) -> Result<Option<TaskSummary>> {
        let tasks = self.list_tasks().await;
        if let Some(task) = tasks.first() {
            return Ok(Some(task.clone()));
        }
        match self.create_task(None, None, None).await {
            Ok(task) => Ok(Some(task)),
            Err(RuntimeError::Store(mai_store::StoreError::InvalidConfig(_))) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn ensure_default_environment(
        self: &Arc<Self>,
    ) -> Result<Option<EnvironmentSummary>> {
        let environments = self.list_environments().await;
        if let Some(environment) = environments.first() {
            return Ok(Some(environment.clone()));
        }
        self.create_environment(
            tasks::DEFAULT_ENVIRONMENT_NAME.to_string(),
            Some(self.deps.docker.image().to_string()),
        )
        .await
        .map(Some)
    }

    pub async fn create_environment(
        self: &Arc<Self>,
        name: String,
        docker_image: Option<String>,
    ) -> Result<EnvironmentSummary> {
        tasks::create_environment(
            &self.state,
            self,
            tasks::CreateEnvironmentInput { name, docker_image },
        )
        .await
    }

    pub async fn get_environment(
        self: &Arc<Self>,
        environment_id: EnvironmentId,
        session_id: Option<SessionId>,
    ) -> Result<EnvironmentDetail> {
        tasks::get_environment(&self.state, self, environment_id, session_id).await
    }

    pub async fn create_environment_conversation(
        self: &Arc<Self>,
        environment_id: EnvironmentId,
    ) -> Result<AgentSessionSummary> {
        tasks::create_environment_conversation(&self.state, self, environment_id).await
    }

    pub async fn send_environment_message(
        self: &Arc<Self>,
        environment_id: EnvironmentId,
        session_id: SessionId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        tasks::send_environment_message(
            &self.state,
            self,
            environment_id,
            session_id,
            message,
            skill_mentions,
        )
        .await
    }

    pub async fn create_task(
        self: &Arc<Self>,
        title: Option<String>,
        initial_message: Option<String>,
        docker_image: Option<String>,
    ) -> Result<TaskSummary> {
        tasks::create_task(
            &self.state,
            self,
            tasks::CreateTaskInput {
                title,
                initial_message,
                docker_image,
            },
        )
        .await
    }

    pub(super) async fn generate_task_title(self: &Arc<Self>, message: &str) -> Result<String> {
        let planner_model = self.resolve_role_agent_model(AgentRole::Planner).await?;
        let selection = self
            .resolve_provider_selection(
                Some(&planner_model.preference.provider_id),
                Some(&planner_model.preference.model),
            )
            .await?;
        let instructions = "Generate a concise task title of 3-8 words that captures the essence of the user's request. Output only the title text, nothing else. Do not use quotes or punctuation at the end.";
        let provider = model_profile::core_provider_for_selection(&selection)?;
        let request =
            model_profile::core_model_turn_request(&selection, None, instructions, Vec::new());
        let title = pl_core::stream_history_completion_message_text(
            provider,
            vec![pl_core::user_text_message(message)],
            request,
            pl_core::CoreModelTurnOptions::default().with_cancellation(CancellationToken::new()),
        )
        .await?;
        let title = title.trim().to_string();
        if title.is_empty() {
            return Ok("New Task".to_string());
        }
        let title = if title.len() > 100 {
            title.chars().take(100).collect()
        } else {
            title
        };
        Ok(title)
    }

    pub(super) async fn update_task_title(
        self: &Arc<Self>,
        task_id: TaskId,
        new_title: String,
    ) -> Result<()> {
        tasks::update_task_title(&self.state, self.as_ref(), task_id, new_title).await
    }

    pub async fn get_task(
        &self,
        task_id: TaskId,
        selected_agent_id: Option<AgentId>,
    ) -> Result<TaskDetail> {
        tasks::get_task(&self.state, self, task_id, selected_agent_id).await
    }

    pub async fn get_project(
        &self,
        project_id: ProjectId,
        selected_agent_id: Option<AgentId>,
        session_id: Option<SessionId>,
    ) -> Result<ProjectDetail> {
        projects::service::get_project(&self.state, self, project_id, selected_agent_id, session_id)
            .await
    }

    pub async fn list_project_review_runs(
        &self,
        project_id: ProjectId,
        offset: usize,
        limit: usize,
    ) -> Result<ProjectReviewRunsResponse> {
        self.project(project_id).await?;
        projects::review::runs::list_project_review_runs(
            &self.deps.store,
            project_id,
            projects::review::cleanup::PROJECT_REVIEW_HISTORY_RETENTION_DAYS,
            offset,
            limit,
        )
        .await
    }

    pub async fn get_project_review_run(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
    ) -> Result<ProjectReviewRunDetail> {
        self.project(project_id).await?;
        projects::review::runs::get_project_review_run(&self.deps.store, project_id, run_id).await
    }

    pub async fn list_project_review_jobs(
        &self,
        project_id: ProjectId,
        offset: usize,
        limit: usize,
    ) -> Result<ProjectReviewJobsResponse> {
        self.project(project_id).await?;
        Ok(ProjectReviewJobsResponse {
            jobs: self
                .deps
                .store
                .load_project_review_jobs(project_id, offset, limit)
                .await?,
        })
    }

    pub async fn get_project_review_job(
        &self,
        project_id: ProjectId,
        job_id: Uuid,
    ) -> Result<ProjectReviewJobDetail> {
        let summary = self
            .deps
            .store
            .load_project_review_job(project_id, job_id)
            .await?
            .ok_or(RuntimeError::ProjectReviewJobNotFound(job_id))?;
        let attempts = self
            .deps
            .store
            .load_project_review_job_attempts(job_id)
            .await?;
        Ok(ProjectReviewJobDetail { summary, attempts })
    }

    pub async fn create_project(
        self: &Arc<Self>,
        request: CreateProjectRequest,
    ) -> Result<ProjectSummary> {
        projects::service::create_project(self, request).await
    }

    pub async fn update_project(
        self: &Arc<Self>,
        project_id: ProjectId,
        request: UpdateProjectRequest,
    ) -> Result<ProjectSummary> {
        projects::service::update_project(&self.state, self, project_id, request).await
    }

    pub async fn delete_project(self: &Arc<Self>, project_id: ProjectId) -> Result<()> {
        projects::service::delete_project(&self.state, self, project_id).await
    }

    pub async fn cancel_project(self: &Arc<Self>, project_id: ProjectId) -> Result<()> {
        projects::service::cancel_project(&self.state, self, project_id).await
    }

    pub async fn send_project_message(
        self: &Arc<Self>,
        project_id: ProjectId,
        request: SendMessageRequest,
    ) -> Result<TurnId> {
        projects::service::send_project_message(&self.state, self, project_id, request).await
    }

    pub async fn publish_external_event(&self, kind: MaiProductEventKind) {
        self.events.publish(kind).await;
    }

    pub async fn find_project_for_github_event(
        &self,
        installation_id: Option<u64>,
        repository_id: Option<u64>,
        repository_full_name: Option<&str>,
    ) -> Option<ProjectId> {
        projects::service::find_project_for_github_event(
            &self.state,
            installation_id,
            repository_id,
            repository_full_name,
        )
        .await
    }

    pub async fn handle_project_push_event(
        self: &Arc<Self>,
        project_id: ProjectId,
        payload: &Value,
    ) -> Result<()> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let pushed_ref = payload
            .get("ref")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let default_ref = format!("refs/heads/{}", summary.branch);
        if pushed_ref == default_ref
            && summary.status == ProjectStatus::Ready
            && summary.clone_status == ProjectCloneStatus::Ready
        {
            self.sync_project_repository(
                project_id,
                projects::workspace::ProjectRepositorySyncTarget::DefaultBranch,
            )
            .await?;
            self.refresh_project_skill_cache_from_project_repository(project_id)
                .await?;
            self.events
                .publish(MaiProductEventKind::ProjectUpdated { project: summary })
                .await;
        }
        Ok(())
    }

    pub async fn enqueue_project_review(
        self: &Arc<Self>,
        request: ProjectReviewQueueRequest,
    ) -> Result<ProjectReviewQueueSummary> {
        let project_id = request.project_id;
        let summary = self
            .enqueue_project_review_signals_with_admission(
                project_id,
                vec![ProjectReviewSignalInput {
                    pr: request.pr,
                    head_sha: request.head_sha.clone(),
                    delivery_id: request.delivery_id.clone(),
                    reason: request.reason.clone(),
                }],
                ProjectReviewEnqueueAdmission::ManualRequest,
                true,
            )
            .await?;
        tracing::info!(
            project_id = %project_id,
            pr = request.pr,
            delivery_id = request.delivery_id.as_deref().unwrap_or_default(),
            reason = %request.reason,
            queued = ?summary.queued,
            deduped = ?summary.deduped,
            ignored = ?summary.ignored,
            "queued project review signal"
        );
        Ok(summary)
    }

    pub async fn enqueue_project_review_relay_signal(
        self: &Arc<Self>,
        request: ProjectReviewQueueRequest,
    ) -> Result<ProjectReviewQueueSummary> {
        let project_id = request.project_id;
        let project = self.project(project_id).await?;
        if !project.summary.read().await.auto_review_enabled {
            return Ok(ProjectReviewQueueSummary {
                ignored: vec![request.pr],
                ..Default::default()
            });
        }
        let selected = projects::review::eligibility::select_project_review_pr(
            self,
            project_id,
            request.pr,
            request.head_sha,
        )
        .await?;
        let summary = match selected {
            Some(selected) => {
                self.enqueue_project_review_signals_with_admission(
                    project_id,
                    vec![ProjectReviewSignalInput {
                        pr: selected.pr,
                        head_sha: selected.head_sha,
                        delivery_id: request.delivery_id.clone(),
                        reason: request.reason.clone(),
                    }],
                    ProjectReviewEnqueueAdmission::RequireAutoReviewEnabled,
                    true,
                )
                .await?
            }
            None => ProjectReviewQueueSummary {
                ignored: vec![request.pr],
                ..Default::default()
            },
        };
        tracing::info!(
            project_id = %project_id,
            pr = request.pr,
            delivery_id = request.delivery_id.as_deref().unwrap_or_default(),
            reason = %request.reason,
            queued = ?summary.queued,
            deduped = ?summary.deduped,
            ignored = ?summary.ignored,
            "queued project review relay signal"
        );
        Ok(summary)
    }

    pub(super) async fn enqueue_project_review_signals(
        self: &Arc<Self>,
        project_id: ProjectId,
        signals: Vec<ProjectReviewSignalInput>,
        start_worker: bool,
    ) -> Result<ProjectReviewQueueSummary> {
        self.enqueue_project_review_signals_with_admission(
            project_id,
            signals,
            ProjectReviewEnqueueAdmission::RequireAutoReviewEnabled,
            start_worker,
        )
        .await
    }

    async fn enqueue_project_review_signals_with_admission(
        self: &Arc<Self>,
        project_id: ProjectId,
        signals: Vec<ProjectReviewSignalInput>,
        admission: ProjectReviewEnqueueAdmission,
        start_worker: bool,
    ) -> Result<ProjectReviewQueueSummary> {
        let project = self.project(project_id).await?;
        {
            let summary = project.summary.read().await;
            if admission == ProjectReviewEnqueueAdmission::RequireAutoReviewEnabled
                && !summary.auto_review_enabled
            {
                return Ok(ProjectReviewQueueSummary {
                    ignored: signals.iter().map(|signal| signal.pr).collect(),
                    ..Default::default()
                });
            }
        }
        let signals_for_events = signals.clone();
        let mut summary = ProjectReviewQueueSummary::default();
        let source = match admission {
            ProjectReviewEnqueueAdmission::ManualRequest => ProjectReviewJobSource::Manual,
            ProjectReviewEnqueueAdmission::RequireAutoReviewEnabled => {
                ProjectReviewJobSource::Automatic
            }
        };
        for signal in signals {
            if signal.pr == 0 {
                summary.ignored.push(signal.pr);
                continue;
            }
            if admission == ProjectReviewEnqueueAdmission::ManualRequest
                && let Some(job) = self
                    .deps
                    .store
                    .load_active_project_review_job_for_pr(project_id, signal.pr)
                    .await?
            {
                summary.deduped.push(job.pr);
                summary.jobs.push(job);
                continue;
            }
            let head_sha = match signal
                .head_sha
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                Some(head_sha) => head_sha.to_string(),
                None => {
                    projects::review::target::resolve_project_review_target(
                        self,
                        project_id,
                        projects::review::target::ProjectReviewRequest {
                            pr: signal.pr,
                            head_sha_hint: None,
                        },
                    )
                    .await?
                    .head_sha
                }
            };
            let job_source = if signal.delivery_id.is_some() {
                ProjectReviewJobSource::Webhook
            } else {
                source.clone()
            };
            let queued = self
                .deps
                .store
                .enqueue_project_review_job(projects::review::job::new_project_review_job(
                    projects::review::job::NewProjectReviewJob {
                        project_id,
                        pr: signal.pr,
                        head_sha,
                        source: job_source,
                        delivery_id: signal.delivery_id,
                        reason: signal.reason,
                    },
                ))
                .await?;
            match queued.disposition {
                mai_store::ProjectReviewJobEnqueueDisposition::Queued => {
                    summary.queued.push(queued.job.pr);
                }
                mai_store::ProjectReviewJobEnqueueDisposition::Deduped => {
                    summary.deduped.push(queued.job.pr);
                }
            }
            summary.jobs.push(queued.job);
        }
        if !summary.queued.is_empty() || !summary.deduped.is_empty() {
            for signal in signals_for_events.iter().filter(|signal| {
                summary.queued.contains(&signal.pr) || summary.deduped.contains(&signal.pr)
            }) {
                self.events
                    .publish(MaiProductEventKind::ProjectReviewQueued {
                        project_id,
                        delivery_id: signal.delivery_id.clone().unwrap_or_default(),
                        pr: signal.pr,
                        reason: signal.reason.clone(),
                    })
                    .await;
            }
            if let Some(job) = self
                .deps
                .store
                .load_active_project_review_job(project_id)
                .await?
            {
                self.set_project_review_state(
                    project_id,
                    projects::review::job::project_review_status_for_job(true, Some(&job)),
                    projects::review::state::ReviewStateUpdate {
                        current_reviewer_agent_id: job.reviewer_agent_id.map_or(
                            projects::review::state::ReviewerAgentUpdate::Keep,
                            projects::review::state::ReviewerAgentUpdate::Set,
                        ),
                        next_review_at: job.next_attempt_at,
                        error: job.failure.as_ref().map(|failure| failure.message.clone()),
                        ..Default::default()
                    },
                )
                .await?;
            }
            project.review_notify.notify_one();
            if start_worker
                && let Err(err) = self.start_project_review_loop_if_ready(project_id).await
            {
                tracing::warn!(
                    project_id = %project_id,
                    "failed to start project review loop after queueing PR signal: {err}"
                );
            }
        }
        Ok(summary)
    }

    pub async fn send_task_message(
        self: &Arc<Self>,
        task_id: TaskId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        tasks::send_task_message(&self.state, self, task_id, message, skill_mentions).await
    }

    pub async fn approve_task_plan(self: &Arc<Self>, task_id: TaskId) -> Result<TaskSummary> {
        tasks::approve_task_plan(&self.state, self, task_id).await
    }

    pub async fn request_plan_revision(
        self: &Arc<Self>,
        task_id: TaskId,
        feedback: String,
    ) -> Result<TaskSummary> {
        tasks::request_plan_revision(&self.state, self, task_id, feedback).await
    }
}
