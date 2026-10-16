//! 统一容器入口：所有角色和来源都先解析期望参考快照，再核对真实环境。

use super::*;
use mai_docker::{AgentEnvironmentPlan, ContainerMount, WorkspaceMountPlan};
use mai_references::{AgentEnvironmentLease, ReferenceError, plan_from_environment};

impl AgentRuntime {
    /// 仅从已持有 lifecycle 锁的 AgentContainerOps 装配入口调用。
    pub(super) async fn start_reference_environment(
        &self,
        request: agents::AgentContainerStartRequest,
    ) -> Result<ContainerHandle> {
        let admission = self.references.admissions.agent(request.agent_id);
        if admission.is_retired() {
            return Err(ReferenceError::new(
                ReferenceErrorCode::EnvironmentSuperseded,
                "Agent 正在退役，不能装配执行环境",
            )
            .into());
        }
        if admission.pending_revision().is_some() {
            return Err(ReferenceError::new(
                ReferenceErrorCode::EnvironmentNotReady,
                "执行环境有尚未完成的应用收据，只能通过应用或恢复流程发布",
            )
            .into());
        }
        let agent = self.agent(request.agent_id).await?;
        let summary = agent.summary.read().await.clone();
        let project_lock = summary
            .project_id
            .map(|id| self.references.project_lock(id));
        // 统一锁序为 Agent lifecycle → 项目短锁；项目锁不跨扫描、Docker 或 MCP 等慢操作。
        let configuration = {
            let _project = match &project_lock {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            match summary.project_id {
                Some(id) => {
                    let project = projects::service::project(&self.state, id).await?;
                    match project.summary.read().await.status {
                        ProjectStatus::Deleting | ProjectStatus::Failed => {
                            return Err(ReferenceError::new(
                                ReferenceErrorCode::EnvironmentSuperseded,
                                "项目正在删除或启动失败，不能装配执行环境",
                            )
                            .into());
                        }
                        ProjectStatus::Creating | ProjectStatus::Ready => {}
                    }
                    self.deps.store.project_reference_configuration(id).await?
                }
                None => ProjectReferenceConfiguration::empty(),
            }
        };
        let snapshot = Arc::new(
            self.references
                .catalog
                .snapshot(summary.project_id.unwrap_or(Uuid::nil()), &configuration)
                .await?,
        );
        let binding = self.deps.store.reference_binding(request.agent_id).await?;
        // Agent 记录的 UUID 同时是首次创建意图：崩溃后可找回同名 generation，不能随机重造。
        let generation = binding
            .as_ref()
            .map_or(request.agent_id, |binding| binding.generation);
        let discovered = if binding.is_none() && request.preferred_container_id.is_none() {
            self.deps
                .docker
                .list_managed_containers()
                .await?
                .into_iter()
                .find(|container| {
                    !container.sidecar
                        && container.agent_id.as_deref()
                            == Some(request.agent_id.to_string().as_str())
                        && container.generation == Some(generation)
                })
                .map(|container| container.id)
        } else {
            None
        };
        let preferred = binding
            .as_ref()
            .map(|binding| binding.container_id.as_str())
            .or(request.preferred_container_id.as_deref())
            .or(discovered.as_deref());
        let existing = match preferred {
            Some(id) => Some(
                self.deps
                    .docker
                    .inspect_container_environment(id)
                    .await
                    .map_err(|error| {
                        ReferenceError::new(
                            ReferenceErrorCode::EnvironmentRecoveryUnavailable,
                            "已登记的执行环境不存在，拒绝退回基础镜像以免丢失软件和数据",
                        )
                        .with_cause(error)
                    })?,
            ),
            None => None,
        };
        let _snapshot_resources = self.references.begin_snapshot_creation().await;
        let (container, image_id) = match existing {
            Some(facts) => {
                if discovered.is_some() {
                    let mut summary = summary.clone();
                    summary.container_id = Some(facts.id.clone());
                    self.deps
                        .store
                        .save_agent(&summary, agent.system_prompt.as_deref())
                        .await?;
                    agent.summary.write().await.container_id = Some(facts.id.clone());
                }
                let image_id = binding.as_ref().map_or(facts.image_id.clone(), |binding| {
                    binding.effective_image_id.clone()
                });
                let plan = plan_from_environment(
                    request.agent_id,
                    facts.generation.map(|_| generation),
                    &image_id,
                    &snapshot,
                    &facts,
                )?;
                // Config.Image 可以保留原始 tag；实际镜像 ID 单独核对。
                let plan = with_original_image_reference(plan, &facts.image_reference)?;
                match self.deps.docker.verify_environment(&plan, &facts.id).await {
                    Ok(verified) => (
                        self.deps
                            .docker
                            .prepare_verified_container(&verified)
                            .await?,
                        image_id,
                    ),
                    Err(error) => {
                        if let Some(project_id) = summary.project_id {
                            self.deps
                                .store
                                .enqueue_reference_application(
                                    project_id,
                                    (*snapshot).clone(),
                                    ReferenceApplyPolicy::WhenIdle,
                                    mai_store::ReferenceApplyScope::AllPhysicalEnvironments,
                                )
                                .await?;
                            admission.block(configuration.revision);
                        }
                        return Err(ReferenceError::new(
                            ReferenceErrorCode::EnvironmentNotReady,
                            format!("执行环境需要显式重配置：{error}"),
                        )
                        .into());
                    }
                }
            }
            None => {
                let (workspace, volumes) = self
                    .reference_workspace_plan(request.agent_id, &request.source)
                    .await?;
                let image = match &request.source {
                    agents::ContainerSource::CloneFrom {
                        parent_container_id,
                        ..
                    } => {
                        self.deps
                            .docker
                            .commit_clone_parent(
                                request.agent_id.to_string(),
                                generation,
                                parent_container_id,
                            )
                            .await?
                    }
                    agents::ContainerSource::FreshImage
                    | agents::ContainerSource::ProjectWorkspace { .. } => {
                        request.docker_image.clone()
                    }
                    agents::ContainerSource::ProjectReviewWorkspace { .. } => {
                        return Err(RuntimeError::InvalidInput(
                            "审查工作区必须先解析为最终卷计划".into(),
                        ));
                    }
                };
                let image_id = self.deps.docker.image_id(&image).await?;
                let mut builder =
                    AgentEnvironmentPlan::builder(request.agent_id.to_string(), &image, workspace)
                        .generation(generation)
                        .effective_image_id(&image_id)
                        .mounts(volumes)
                        .mounts(mai_references::reference_mounts(&snapshot)?);
                if let Some(project_id) = summary.project_id {
                    builder = builder.project_id(project_id.to_string());
                }
                let plan = builder.build()?;
                let container = self.deps.docker.create_environment(&plan, &[]).await?;
                let prepared: Result<()> = async {
                    let mut projection = summary.clone();
                    projection.container_id = Some(container.id.clone());
                    self.deps
                        .store
                        .save_agent(&projection, agent.system_prompt.as_deref())
                        .await?;
                    agent.summary.write().await.container_id = Some(container.id.clone());
                    let facts = self
                        .deps
                        .docker
                        .inspect_container_environment(&container.id)
                        .await?;
                    // 镜像声明的匿名卷在创建后才有名称；核对并在换代中复用真实名称。
                    let final_plan = plan_from_environment(
                        request.agent_id,
                        Some(generation),
                        &image_id,
                        &snapshot,
                        &facts,
                    )?;
                    let final_plan = with_original_image_reference(final_plan, &image)?;
                    self.deps
                        .docker
                        .verify_environment(&final_plan, &container.id)
                        .await?;
                    Ok(())
                }
                .await;
                if let Err(error) = prepared {
                    self.deps.docker.stop_container(&container.id).await?;
                    return Err(error);
                }
                (container, image_id)
            }
        };
        let binding = mai_store::PersistedReferenceBinding {
            agent_id: request.agent_id,
            project_id: summary.project_id,
            generation,
            container_id: container.id.clone(),
            effective_image_id: image_id,
            applied_revision: configuration.revision,
            fingerprint: snapshot.fingerprint.clone(),
            snapshot: (*snapshot).clone(),
            updated_at: now(),
        };
        let publication: Result<()> = async {
            let _project = match &project_lock {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            if let Some(project_id) = summary.project_id {
                let current = self
                    .deps
                    .store
                    .project_reference_configuration(project_id)
                    .await?;
                if current.revision != configuration.revision {
                    return Err(ReferenceError::new(
                        ReferenceErrorCode::ReferenceRevisionConflict,
                        "装配期间项目引用已更新，执行环境等待应用最新修订",
                    )
                    .into());
                }
            }
            self.deps.store.publish_reference_binding(binding).await?;
            self.references
                .leases
                .publish(AgentEnvironmentLease::new(
                    request.agent_id,
                    &container.id,
                    generation,
                    snapshot,
                ))
                .await?;
            admission.published(configuration.revision);
            Ok(())
        }
        .await;
        if let Err(error) = publication {
            if let Err(cleanup) = self.references.leases.remove(request.agent_id).await {
                tracing::error!(%cleanup, "首次环境发布失败后租约未收敛");
            }
            self.deps.docker.stop_container(&container.id).await?;
            // 首次容器在短锁之外创建；配置抢先提交时补上该物理环境的 durable 应用意图。
            if let Some(project_id) = summary.project_id {
                admission.block(configuration.revision);
                if let Ok(current) = self
                    .deps
                    .store
                    .project_reference_configuration(project_id)
                    .await
                    && let Ok(target) = self.references.catalog.snapshot(project_id, &current).await
                    && let Err(enqueue) = self
                        .deps
                        .store
                        .enqueue_reference_application(
                            project_id,
                            target,
                            ReferenceApplyPolicy::WhenIdle,
                            mai_store::ReferenceApplyScope::AllPhysicalEnvironments,
                        )
                        .await
                {
                    tracing::error!(%enqueue, "首次环境发布失败后应用意图未能持久化，保持阻塞");
                }
            }
            return Err(error);
        }
        Ok(container)
    }

    async fn reference_workspace_plan(
        &self,
        agent_id: AgentId,
        source: &agents::ContainerSource,
    ) -> Result<(WorkspaceMountPlan, Vec<ContainerMount>)> {
        match source {
            agents::ContainerSource::FreshImage => Ok((
                WorkspaceMountPlan::agent_default(&agent_id.to_string())?,
                Vec::new(),
            )),
            agents::ContainerSource::ProjectWorkspace {
                workspace_volume,
                repo_path,
                repository_view,
            } => {
                if repo_path != projects::workspace::AGENT_WORKSPACE_REPO_PATH {
                    return Err(RuntimeError::InvalidInput("项目工作目录不合法".into()));
                }
                self.ensure_agent_workspace_volume(agent_id, workspace_volume, None)
                    .await?;
                let mounts = repository_view
                    .as_ref()
                    .map(|view| {
                        ContainerVolumeMount::read_only_subpath(
                            &view.volume,
                            &view.container_path,
                            &view.volume_subpath,
                        )
                        .map(ContainerMount::Volume)
                    })
                    .transpose()?
                    .into_iter()
                    .collect();
                Ok((
                    WorkspaceMountPlan::new(workspace_volume, None::<String>)?,
                    mounts,
                ))
            }
            agents::ContainerSource::CloneFrom {
                workspace_volume, ..
            } => {
                let name = workspace_volume
                    .clone()
                    .unwrap_or_else(|| agent_workspace_volume(&agent_id.to_string()));
                self.ensure_agent_workspace_volume(agent_id, &name, None)
                    .await?;
                Ok((WorkspaceMountPlan::new(name, None::<String>)?, Vec::new()))
            }
            agents::ContainerSource::ProjectReviewWorkspace { .. } => {
                Err(RuntimeError::InvalidInput("审查工作区尚未解析".into()))
            }
        }
    }
}

fn with_original_image_reference(
    plan: AgentEnvironmentPlan,
    image: &str,
) -> Result<AgentEnvironmentPlan> {
    let mut builder =
        AgentEnvironmentPlan::builder(plan.agent_id(), image, plan.workspace().clone())
            .effective_image_id(plan.effective_image_id())
            .mounts(plan.additional_mounts().iter().cloned())
            .options(plan.options().clone())
            .network(plan.network());
    if let Some(generation) = plan.generation() {
        builder = builder.generation(generation);
    }
    if let Some(project_id) = plan.project_id() {
        builder = builder.project_id(project_id);
    }
    Ok(builder.build()?)
}
