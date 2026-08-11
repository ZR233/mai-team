use super::*;

impl AgentRuntime {
    pub async fn create_agent(
        self: &Arc<Self>,
        request: CreateAgentRequest,
    ) -> Result<AgentSummary> {
        self.create_agent_with_container_source(
            request,
            agents::ContainerSource::FreshImage,
            None,
            None,
            None,
        )
        .await
    }

    pub(super) async fn create_agent_with_container_source(
        self: &Arc<Self>,
        request: CreateAgentRequest,
        container_source: agents::ContainerSource,
        task_id: Option<TaskId>,
        project_id: Option<ProjectId>,
        role: Option<AgentRole>,
    ) -> Result<AgentSummary> {
        let resource = self
            .create_agent_resource_with_container_source(
                AgentId::new_v4(),
                request,
                container_source,
                task_id,
                project_id,
                role,
            )
            .await?;
        self.register_prepared_agent(resource).await
    }

    pub(super) async fn register_prepared_agent(
        self: &Arc<Self>,
        mut resource: runtime_agent_creation::PreparedAgentResource,
    ) -> Result<AgentSummary> {
        resource.include_canonical_runtime();
        match self.register_framework_agent(resource.id()).await {
            Ok(()) => {
                let summary = resource.commit();
                self.events
                    .publish(MaiProductEventKind::AgentCreated {
                        agent: summary.clone(),
                    })
                    .await;
                Ok(summary)
            }
            Err(error) => match resource.rollback().await {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(RuntimeError::InvalidInput(format!(
                    "agent framework registration failed: {error}; creation rollback failed: {rollback_error}"
                ))),
            },
        }
    }

    pub(super) async fn create_agent_resource_with_container_source(
        self: &Arc<Self>,
        agent_id: AgentId,
        request: CreateAgentRequest,
        container_source: agents::ContainerSource,
        task_id: Option<TaskId>,
        project_id: Option<ProjectId>,
        role: Option<AgentRole>,
    ) -> Result<runtime_agent_creation::PreparedAgentResource> {
        let created = agents::create_agent_record(
            self.as_ref(),
            request,
            agents::CreateAgentRecordContext {
                id: agent_id,
                task_id,
                project_id,
                role,
            },
        )
        .await?;
        let agent_id = created.summary.id;
        let agent = created.record;
        let mut resource =
            runtime_agent_creation::PreparedAgentResource::new(self, created.summary);
        let provisioning: Result<AgentSummary> = async {
            let container_source = self
                .agent_container_source_for_project(agent_id, project_id, container_source)
                .await?;
            agents::ensure_agent_container_with_source(self.as_ref(), &agent, &container_source)
                .await?;
            Ok(agent.summary.read().await.clone())
        }
        .await;

        match provisioning {
            Ok(summary) => {
                resource.replace_summary(summary);
                Ok(resource)
            }
            Err(err) => {
                let message = err.to_string();
                if let Err(store_err) = self
                    .set_agent_resource_state(
                        &agent,
                        AgentResourceState::Failed,
                        Some(message.clone()),
                    )
                    .await
                {
                    tracing::warn!("failed to persist agent failure: {store_err}");
                }
                self.events
                    .publish(MaiProductEventKind::OperationFailed {
                        scope: "agent_provisioning".to_string(),
                        agent_id: Some(agent_id),
                        message,
                    })
                    .await;
                if let Err(rollback_error) = resource.rollback().await {
                    return Err(RuntimeError::InvalidInput(format!(
                        "agent provisioning failed: {err}; creation rollback failed: {rollback_error}"
                    )));
                }
                Err(err)
            }
        }
    }

    pub(super) async fn agent_container_source_for_project(
        &self,
        agent_id: AgentId,
        project_id: Option<ProjectId>,
        source: agents::ContainerSource,
    ) -> Result<agents::ContainerSource> {
        let Some(project_id) = project_id else {
            return Ok(source);
        };
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        if summary.status != ProjectStatus::Ready
            && summary.clone_status != ProjectCloneStatus::Ready
        {
            return Ok(source);
        }
        let workspace_volume =
            project_agent_workspace_volume(&summary.id.to_string(), &agent_id.to_string());
        let repo_path = projects::workspace::AGENT_WORKSPACE_REPO_PATH.to_string();
        Ok(match source {
            agents::ContainerSource::ProjectReviewWorkspace {
                target,
                revision,
                repository_view,
            } => {
                let agent = self.agent(agent_id).await?;
                if agent.summary.read().await.role != Some(AgentRole::Reviewer) {
                    return Err(RuntimeError::InvalidInput(format!(
                        "project repository snapshot cannot be mounted for non-reviewer agent `{agent_id}`"
                    )));
                }
                if repository_view.volume != project_cache_volume(&summary.id.to_string())
                    || repository_view.base_sha != revision.base_sha
                {
                    return Err(RuntimeError::InvalidInput(
                        "project repository snapshot does not match the prepared review revision"
                            .to_string(),
                    ));
                }
                let token = self.project_git_token(summary.id).await?.ok_or_else(|| {
                    RuntimeError::InvalidInput(
                        "project git account token is not configured".to_string(),
                    )
                })?;
                self.sync_project_review_workspace_volume_from_repository(
                    &summary, agent_id, &target, &revision, &token,
                )
                .await?;
                agents::ContainerSource::ProjectWorkspace {
                    workspace_volume,
                    repo_path,
                    repository_view: Some(repository_view),
                }
            }
            agents::ContainerSource::FreshImage => {
                if !self.deps.docker.volume_exists(&workspace_volume).await? {
                    let token = self.project_git_token(summary.id).await?.ok_or_else(|| {
                        RuntimeError::InvalidInput(
                            "project git account token is not configured".to_string(),
                        )
                    })?;
                    self.sync_agent_workspace_volume_repo(&summary, agent_id, &token)
                        .await?;
                }
                agents::ContainerSource::ProjectWorkspace {
                    workspace_volume,
                    repo_path,
                    repository_view: None,
                }
            }
            agents::ContainerSource::ProjectWorkspace {
                workspace_volume,
                repo_path,
                repository_view,
            } => agents::ContainerSource::ProjectWorkspace {
                workspace_volume,
                repo_path,
                repository_view,
            },
            agents::ContainerSource::CloneFrom {
                parent_container_id,
                docker_image,
                workspace_volume: _,
            } => {
                if !self.deps.docker.volume_exists(&workspace_volume).await? {
                    let token = self.project_git_token(summary.id).await?.ok_or_else(|| {
                        RuntimeError::InvalidInput(
                            "project git account token is not configured".to_string(),
                        )
                    })?;
                    self.sync_agent_workspace_volume_repo(&summary, agent_id, &token)
                        .await?;
                }
                agents::ContainerSource::CloneFrom {
                    parent_container_id,
                    docker_image,
                    workspace_volume: Some(workspace_volume),
                }
            }
        })
    }

    pub async fn list_agents(&self) -> Vec<AgentSummary> {
        let agents = self.state.agents.read().await.values().cloned().collect();
        agents::list_agents(agents).await
    }

    pub(super) async fn reconcile_project_workspaces(self: &Arc<Self>) -> Result<()> {
        let projects = self.list_projects().await;
        let agents = self.list_agents().await;
        let report = self.workspace_manager.reconcile(&projects, &agents).await?;
        if !report.orphan_clones_removed.is_empty() {
            tracing::info!(
                count = report.orphan_clones_removed.len(),
                "removed orphan project clone directories during startup reconcile"
            );
        }
        if !report.orphan_clone_removal_failed.is_empty() {
            tracing::warn!(
                count = report.orphan_clone_removal_failed.len(),
                "failed to remove orphan project clone directories during startup reconcile"
            );
        }
        if !report.orphan_project_dirs_archived.is_empty() {
            tracing::info!(
                count = report.orphan_project_dirs_archived.len(),
                "archived orphan project directories during startup reconcile"
            );
        }
        if !report.legacy_worktree_dirs_archived.is_empty() {
            tracing::info!(
                count = report.legacy_worktree_dirs_archived.len(),
                "archived legacy project worktree directories during startup reconcile"
            );
        }
        if !report.invalid_clone_dirs.is_empty() {
            tracing::warn!(
                count = report.invalid_clone_dirs.len(),
                "found invalid project clone directories during startup reconcile"
            );
        }

        let volume_report = projects::workspace::docker_reconcile::reconcile_project_volumes(
            &self.deps.docker,
            &projects,
            &agents,
        )
        .await?;
        if !volume_report
            .orphan_agent_workspace_volumes_removed
            .is_empty()
        {
            tracing::info!(
                count = volume_report.orphan_agent_workspace_volumes_removed.len(),
                "removed orphan project agent workspace volumes during startup reconcile"
            );
        }
        if !volume_report
            .orphan_agent_workspace_volume_removal_failed
            .is_empty()
        {
            tracing::warn!(
                count = volume_report
                    .orphan_agent_workspace_volume_removal_failed
                    .len(),
                "failed to remove orphan project agent workspace volumes during startup reconcile"
            );
        }
        if !volume_report
            .orphan_project_cache_volumes_removed
            .is_empty()
        {
            tracing::info!(
                count = volume_report.orphan_project_cache_volumes_removed.len(),
                "removed orphan project cache volumes during startup reconcile"
            );
        }
        if !volume_report
            .orphan_project_cache_volume_removal_failed
            .is_empty()
        {
            tracing::warn!(
                count = volume_report
                    .orphan_project_cache_volume_removal_failed
                    .len(),
                "failed to remove orphan project cache volumes during startup reconcile"
            );
        }
        if !volume_report
            .legacy_agent_workspace_volumes_present
            .is_empty()
        {
            tracing::warn!(
                count = volume_report.legacy_agent_workspace_volumes_present.len(),
                "accepted legacy project agent workspace volumes without managed labels during startup reconcile"
            );
        }
        if !volume_report
            .legacy_project_cache_volumes_present
            .is_empty()
        {
            tracing::warn!(
                count = volume_report.legacy_project_cache_volumes_present.len(),
                "accepted legacy project cache volumes without managed labels during startup reconcile"
            );
        }
        if !volume_report.quarantined_volumes.is_empty() {
            tracing::warn!(
                count = volume_report.quarantined_volumes.len(),
                volumes = ?volume_report.quarantined_volumes,
                "quarantined Mai volumes with invalid names or conflicting ownership labels"
            );
        }
        if !volume_report.attached_orphan_volumes.is_empty() {
            tracing::warn!(
                count = volume_report.attached_orphan_volumes.len(),
                volumes = ?volume_report.attached_orphan_volumes,
                "quarantined orphan Mai volumes that are still attached"
            );
        }
        let workspace_projects_to_resume = projects
            .iter()
            .filter(|project| project_workspace_needs_startup_resume(project))
            .map(|project| project.id)
            .collect::<HashSet<_>>();
        if !volume_report.missing_project_cache_volumes.is_empty() {
            tracing::warn!(
                count = volume_report.missing_project_cache_volumes.len(),
                "found projects with missing canonical repository volumes during startup reconcile"
            );
        }
        if !workspace_projects_to_resume.is_empty() {
            tracing::warn!(
                count = workspace_projects_to_resume.len(),
                "found project workspaces requiring startup resume"
            );
        }
        if !workspace_projects_to_resume.is_empty() {
            for project_id in &workspace_projects_to_resume {
                let Some(project_summary) =
                    projects.iter().find(|project| project.id == *project_id)
                else {
                    continue;
                };
                self.start_project_workspace(*project_id, project_summary.maintainer_agent_id)
                    .await?;
            }
        }
        if !volume_report.missing_agent_workspace_volumes.is_empty() {
            tracing::warn!(
                count = volume_report.missing_agent_workspace_volumes.len(),
                "found project agents with missing workspace volumes during startup reconcile"
            );
            for agent_id in &volume_report.missing_agent_workspace_volumes {
                let Some(agent_summary) = agents.iter().find(|agent| agent.id == *agent_id) else {
                    continue;
                };
                let Some(project_id) = agent_summary.project_id else {
                    continue;
                };
                if workspace_projects_to_resume.contains(&project_id) {
                    continue;
                }
                let agent = self.agent(*agent_id).await?;
                self.set_agent_resource_state(
                    &agent,
                    AgentResourceState::Failed,
                    Some(
                        PROJECT_AGENT_WORKSPACE_VOLUME_MISSING_AFTER_STARTUP_RECONCILE.to_string(),
                    ),
                )
                .await?;
            }
        }
        let mut recovered_agent_count = 0_usize;
        for agent_summary in &agents {
            let error = agent_summary
                .state
                .resource_error
                .as_deref()
                .map(str::trim)
                .map(|error| error.strip_prefix("invalid input: ").unwrap_or(error));
            if agent_summary.state.resource != AgentResourceState::Failed
                || agent_summary.state.runtime.active_turn.is_some()
                || error != Some(PROJECT_AGENT_WORKSPACE_VOLUME_MISSING_AFTER_STARTUP_RECONCILE)
            {
                continue;
            }
            let Some(project_id) = agent_summary.project_id else {
                continue;
            };
            let workspace_volume = project_agent_workspace_volume(
                &project_id.to_string(),
                &agent_summary.id.to_string(),
            );
            if !self.deps.docker.volume_exists(&workspace_volume).await? {
                continue;
            }
            let agent = self.agent(agent_summary.id).await?;
            self.set_agent_resource_state(&agent, AgentResourceState::Ready, None)
                .await?;
            recovered_agent_count += 1;
        }
        if recovered_agent_count > 0 {
            tracing::warn!(
                count = recovered_agent_count,
                "recovered project agents failed by missing workspace volume after startup reconcile"
            );
        }
        Ok(())
    }
}
