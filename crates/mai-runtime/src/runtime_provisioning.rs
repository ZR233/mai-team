use super::*;
use std::sync::atomic::Ordering;

use pl_protocol::AgentWorkspaceAssignmentSnapshot;

/// 创建产品 Agent 记录时一并冻结的协作身份。
///
/// 普通创建路径使用角色的产品 Profile；协作 `spawn_agent` 用它覆盖角色 Profile，
/// 并冻结 child 的工作区边界，使后续恢复不再依赖当时的配置。
#[derive(Debug, Clone, Default)]
pub(crate) struct AgentCreationIdentity {
    /// child 冻结的产品 Profile id。
    pub(crate) profile_id: Option<String>,
    /// child 冻结的工作区边界收据。
    pub(crate) workspace: Option<AgentWorkspaceAssignmentSnapshot>,
}

/// 一次完整的产品 Agent 创建请求。
///
/// 普通创建路径的 `identity` 为空；协作 `spawn_agent` 把 child 冻结的 Profile 与工作区边界一并
/// 交给创建事务，避免创建后再补写产品事实。
pub(crate) struct AgentCreationRequest {
    pub(crate) agent_id: AgentId,
    pub(crate) request: CreateAgentRequest,
    pub(crate) container_source: agents::ContainerSource,
    pub(crate) task_id: Option<TaskId>,
    pub(crate) project_id: Option<ProjectId>,
    pub(crate) role: Option<AgentRole>,
    pub(crate) identity: AgentCreationIdentity,
}

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
        let registered = self.register_prepared_thread(&mut resource).await;
        self.finish_agent_registration(resource, registered).await
    }

    /// 注册一个已经准备好产品资源的协作 child，并用调用方冻结的上下文继承装配新 Thread。
    ///
    /// 与普通创建唯一的区别是 Thread 的初始 context：child 先写入自己的 Profile 指令，再追加
    /// 由 [`pl_core::context::ContextSnapshot::inherit`] 选出的调用方记录。注册失败时整棵创建
    /// 被回滚，不会留下半成品 child。
    pub(super) async fn register_prepared_child_agent(
        self: &Arc<Self>,
        mut resource: runtime_agent_creation::PreparedAgentResource,
        caller: &pl_core::tool::opaque::CallContext,
        inheritance: pl_core::context::ContextInheritance,
    ) -> Result<AgentSummary> {
        let registered = self
            .register_prepared_child_thread(&mut resource, caller, inheritance)
            .await;
        self.finish_agent_registration(resource, registered).await
    }

    /// 统一收尾：注册成功后提交创建租约并公布事件，失败则回滚全部产品资源。
    async fn finish_agent_registration(
        self: &Arc<Self>,
        resource: runtime_agent_creation::PreparedAgentResource,
        registered: Result<thread_kernel::ResidentThread>,
    ) -> Result<AgentSummary> {
        match registered {
            Ok(_) => {
                if let Ok(agent) = self.agent(resource.id()).await {
                    agent.registration_pending.store(false, Ordering::Release);
                }
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
                    "Thread registration failed: {error}; creation rollback failed: {rollback_error}"
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
        self.create_agent_resource(AgentCreationRequest {
            agent_id,
            request,
            container_source,
            task_id,
            project_id,
            role,
            identity: AgentCreationIdentity::default(),
        })
        .await
    }

    /// 创建产品 Agent 记录与容器资源，并把冻结的 Profile 身份与工作区边界写入产品事实。
    ///
    /// 身份写入与容器准备同属一次创建事务：任一步失败都会走已有的创建回滚，不留下只写了一半
    /// 产品事实的 Agent。
    pub(super) async fn create_agent_resource(
        self: &Arc<Self>,
        creation: AgentCreationRequest,
    ) -> Result<runtime_agent_creation::PreparedAgentResource> {
        let AgentCreationRequest {
            agent_id,
            request,
            container_source,
            task_id,
            project_id,
            role,
            identity,
        } = creation;
        // Review Thread 身份在创建前就必须进入产品事实：装配发生在
        // review context 附加之前，后续恢复也不会再附加 context。
        let review_run_id = container_source.review_run_id();
        let profile_id = identity.profile_id.unwrap_or_else(|| {
            if review_run_id.is_some() {
                "project-reviewer".to_string()
            } else {
                role.unwrap_or_default().to_string()
            }
        });
        let created = agents::create_agent_record(
            self.as_ref(),
            request,
            agents::CreateAgentRecordContext {
                id: agent_id,
                task_id,
                project_id,
                role,
                review_run_id,
                profile_id,
                workspace: identity.workspace,
            },
        )
        .await?;
        let agent_id = created.summary.id;
        let agent = created.record;
        let mut resource = runtime_agent_creation::PreparedAgentResource::new(
            self,
            agent.summary.read().await.clone(),
        );
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
            || summary.clone_status != ProjectCloneStatus::Ready
        {
            return Ok(source);
        }
        let workspace_volume =
            project_agent_workspace_volume(&summary.id.to_string(), &agent_id.to_string());
        let repo_path = projects::workspace::AGENT_WORKSPACE_REPO_PATH.to_string();
        Ok(match source {
            agents::ContainerSource::ProjectReviewWorkspace {
                run_id: _,
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
        let missing_agent_workspace_volumes = volume_report
            .missing_agent_workspace_volumes
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let mut agent_resources_to_recover = agents
            .iter()
            .filter_map(agents::agent_resource_recovery_retry_request)
            .map(|request| (request.agent_id(), request))
            .collect::<HashMap<_, _>>();
        for agent_id in &missing_agent_workspace_volumes {
            agent_resources_to_recover.insert(
                *agent_id,
                agents::AgentResourceRecoveryRequest::created_workspace(*agent_id),
            );
        }
        if !agent_resources_to_recover.is_empty() {
            tracing::warn!(
                count = agent_resources_to_recover.len(),
                "found project agents requiring derived resource recovery during startup reconcile"
            );
            let mut recovery_requests =
                agent_resources_to_recover.into_values().collect::<Vec<_>>();
            recovery_requests.sort_by_key(|request| request.agent_id());
            for request in recovery_requests {
                let agent_id = request.agent_id();
                let Some(agent_summary) = agents.iter().find(|agent| agent.id == agent_id) else {
                    continue;
                };
                let Some(project_id) = agent_summary.project_id else {
                    continue;
                };
                if workspace_projects_to_resume.contains(&project_id) {
                    continue;
                }
                if let Err(error) = self.recover_project_agent_resources(request).await {
                    tracing::warn!(
                        agent_id = %agent_id,
                        project_id = %project_id,
                        "failed to recover project agent resources during startup: {error}"
                    );
                }
            }
        }
        Ok(())
    }
}
