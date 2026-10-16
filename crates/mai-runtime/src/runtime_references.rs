//! 参考源产品入口：把领域服务接到事件、Agent 生命周期和后台收据恢复。

use super::*;
use mai_references::{ReferenceCatalog, ReferenceCatalogChange};

impl AgentRuntime {
    pub fn reference_catalog(&self) -> Arc<ReferenceCatalog> {
        Arc::clone(&self.references.catalog)
    }

    pub async fn reference_source_policy(&self) -> Result<ReferenceSourcePolicy> {
        Ok(self.references.catalog.deployment_policy()?.public_policy())
    }

    pub async fn project_reference_configuration(
        &self,
        project_id: ProjectId,
    ) -> Result<ProjectReferenceConfiguration> {
        self.project(project_id).await?;
        Ok(self
            .deps
            .store
            .project_reference_configuration(project_id)
            .await?)
    }

    pub async fn replace_project_references(
        &self,
        project_id: ProjectId,
        request: ReplaceProjectReferencesRequest,
    ) -> Result<ProjectReferenceConfiguration> {
        self.project(project_id).await?;
        let saved = self.references.replace(project_id, request).await?;
        self.events
            .publish(MaiProductEventKind::ProjectReferencesChanged {
                project_id,
                revision: saved.revision,
            })
            .await;
        Ok(saved)
    }

    pub async fn apply_project_references(
        &self,
        project_id: ProjectId,
        request: ApplyProjectReferencesRequest,
    ) -> Result<ProjectReferenceStatus> {
        self.project(project_id).await?;
        let policy = request.policy;
        self.references.request_apply(project_id, request).await?;
        if policy == ReferenceApplyPolicy::StopAndApply {
            // 重型换代仍限制为两个；停止请求不能排在其它 Agent 的 commit/create 后面。
            for (agent_id, physical) in self
                .deps
                .store
                .reference_agent_inventory(project_id)
                .await?
            {
                if physical {
                    self.cancel_reference_turn(agent_id).await?;
                }
            }
        }
        self.project_reference_status(project_id, ReferenceStatusQuery::default())
            .await
    }

    pub async fn project_reference_status(
        &self,
        project_id: ProjectId,
        query: ReferenceStatusQuery,
    ) -> Result<ProjectReferenceStatus> {
        let configuration = self.project_reference_configuration(project_id).await?;
        let operations = self.deps.store.reference_operations().await?;
        let mut agents = Vec::new();
        let mut counts = ReferenceApplicationCounts::default();
        for summary in self
            .list_agents()
            .await
            .into_iter()
            .filter(|agent| agent.project_id == Some(project_id))
        {
            let binding = self.deps.store.reference_binding(summary.id).await?;
            let operation = operations
                .iter()
                .rev()
                .find(|receipt| {
                    receipt.operation.agent_id == summary.id
                        && receipt.operation.target_revision == configuration.revision
                })
                .map(|receipt| receipt.operation.clone());
            let mut state = match &operation {
                Some(operation) => match operation.phase {
                    ReferenceApplyPhase::Failed => ReferenceApplicationState::Failed,
                    ReferenceApplyPhase::WaitingIdle => ReferenceApplicationState::WaitingIdle,
                    ReferenceApplyPhase::Draining
                    | ReferenceApplyPhase::Snapshotting
                    | ReferenceApplyPhase::Creating
                    | ReferenceApplyPhase::Verifying
                    | ReferenceApplyPhase::Rebinding
                    | ReferenceApplyPhase::Publishing
                    | ReferenceApplyPhase::CleanupPending => ReferenceApplicationState::Applying,
                    ReferenceApplyPhase::Succeeded | ReferenceApplyPhase::Superseded => {
                        binding_state(binding.as_ref(), &summary, configuration.revision)
                    }
                },
                None => binding_state(binding.as_ref(), &summary, configuration.revision),
            };
            let mut observed_failure = operation
                .as_ref()
                .and_then(|operation| operation.failure.clone());
            if state == ReferenceApplicationState::Applied {
                let physical_id = binding
                    .as_ref()
                    .map(|binding| binding.container_id.as_str())
                    .or(summary.container_id.as_deref());
                if let Some(id) = physical_id {
                    let observation: mai_references::Result<()> = async {
                        let facts = self
                            .deps
                            .docker
                            .inspect_container_environment(id)
                            .await
                            .map_err(|error| {
                                mai_references::ReferenceError::new(
                                    ReferenceErrorCode::EnvironmentRecoveryUnavailable,
                                    "登记的执行环境无法核验",
                                )
                                .with_cause(error)
                            })?;
                        let snapshot = match &binding {
                            Some(binding) => binding.snapshot.clone(),
                            None => {
                                self.references
                                    .catalog
                                    .snapshot(project_id, &configuration)
                                    .await?
                            }
                        };
                        let image = binding.as_ref().map_or(facts.image_id.as_str(), |binding| {
                            binding.effective_image_id.as_str()
                        });
                        let generation = facts.generation.map(|generation| {
                            binding
                                .as_ref()
                                .map_or(generation, |binding| binding.generation)
                        });
                        let plan = mai_references::plan_from_environment(
                            summary.id, generation, image, &snapshot, &facts,
                        )?;
                        let diff = mai_docker::compare_environment(&plan, &facts);
                        if !diff.is_empty() {
                            return Err(mai_references::ReferenceError::new(
                                ReferenceErrorCode::EnvironmentNotReady,
                                format!("实际执行环境不匹配：{diff}"),
                            ));
                        }
                        Ok(())
                    }
                    .await;
                    if let Err(error) = observation {
                        state = ReferenceApplicationState::Failed;
                        observed_failure = Some(mai_references::validation_failure(&error, now()));
                    }
                }
            }
            match state {
                ReferenceApplicationState::Applied => counts.applied += 1,
                ReferenceApplicationState::Failed => counts.failed += 1,
                ReferenceApplicationState::Deferred => counts.deferred += 1,
                ReferenceApplicationState::NoExecutionEnvironment => counts.no_environment += 1,
                ReferenceApplicationState::Pending
                | ReferenceApplicationState::Applying
                | ReferenceApplicationState::WaitingIdle => counts.waiting += 1,
            }
            agents.push(AgentReferenceApplication {
                agent_id: summary.id,
                project_id,
                generation: binding.as_ref().map(|item| item.generation),
                container_id: binding
                    .as_ref()
                    .map(|item| item.container_id.clone())
                    .or(summary.container_id),
                effective_image_id: binding.as_ref().map(|item| item.effective_image_id.clone()),
                applied_revision: binding.as_ref().map_or(0, |item| item.applied_revision),
                applied_fingerprint: binding.as_ref().map(|item| item.fingerprint.clone()),
                state,
                failure: observed_failure,
                operation,
                observed_at: now(),
            });
        }
        agents.sort_by_key(|agent| agent.agent_id);
        let query = query.normalized();
        let total = agents.len() as u64;
        let state = if counts.failed > 0 {
            ProjectReferenceApplicationState::Failed
        } else if counts.waiting > 0 {
            ProjectReferenceApplicationState::Pending
        } else if counts.deferred > 0 {
            ProjectReferenceApplicationState::Deferred
        } else {
            ProjectReferenceApplicationState::Applied
        };
        Ok(ProjectReferenceStatus {
            project_id,
            target_revision: configuration.revision,
            configuration,
            state,
            counts,
            agents: agents
                .into_iter()
                .skip(usize::try_from(query.offset).unwrap_or(usize::MAX))
                .take(query.limit as usize)
                .collect(),
            page: ReferencePageInfo {
                offset: query.offset,
                limit: query.limit,
                total,
            },
            observed_at: now(),
        })
    }

    /// 工具装配取得固定租约；公开文件入口还需检查当前准入状态。
    pub(crate) async fn environment_lease(
        &self,
        agent_id: AgentId,
    ) -> Result<Arc<mai_references::AgentEnvironmentLease>> {
        match self.references.leases.acquire(agent_id).await {
            Ok(lease) => Ok(lease),
            Err(_) => {
                Box::pin(self.container_id(agent_id)).await?;
                Ok(self.references.leases.acquire(agent_id).await?)
            }
        }
    }

    pub(super) async fn start_reference_services(self: &Arc<Self>) -> Result<()> {
        self.references.restore_admission().await?;
        self.references.catalog.resume_validations().await?;
        let shutdown = self.references.shutdown_token();
        let mut sources = self.references.catalog.subscribe();
        let weak = Arc::downgrade(self);
        self.references.spawn_worker(async move { loop { match tokio::select! { result = sources.recv() => result, _ = shutdown.cancelled() => return } {
            Ok(ReferenceCatalogChange::Source { source_id, state }) => {
                let Some(runtime) = weak.upgrade() else { return; };
                runtime.events.publish(MaiProductEventKind::ReferenceSourceChanged { source_id, state }).await;
            },
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        } } });
        let shutdown = self.references.shutdown_token();
        let mut changes = self.references.subscribe();
        let weak = Arc::downgrade(self);
        self.references.spawn_worker(async move { loop { match tokio::select! { result = changes.recv() => result, _ = shutdown.cancelled() => return } {
            Ok((project_id, agent_id, revision, state)) => {
                let Some(runtime) = weak.upgrade() else { return; };
                runtime.events.publish(MaiProductEventKind::AgentReferenceApplicationChanged { project_id, agent_id, revision, state }).await;
            },
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        } } });
        // 恢复屏障先启动收据，再允许原有审查与孤儿清理进入；失败收据保持阻塞。
        let mut latest = HashMap::new();
        for receipt in self.deps.store.reference_operations().await? {
            latest.insert(receipt.operation.agent_id, receipt);
        }
        for receipt in latest.into_values() {
            match receipt.operation.phase {
                ReferenceApplyPhase::Succeeded | ReferenceApplyPhase::Superseded => {}
                ReferenceApplyPhase::Failed => {
                    for id in [
                        &receipt.operation.old_container_id,
                        &receipt.operation.candidate_container_id,
                    ]
                    .into_iter()
                    .flatten()
                    {
                        if self
                            .deps
                            .docker
                            .inspect_container_environment(id)
                            .await
                            .is_ok()
                        {
                            self.deps.docker.stop_container(id).await?;
                        }
                    }
                }
                ReferenceApplyPhase::WaitingIdle
                | ReferenceApplyPhase::Draining
                | ReferenceApplyPhase::Snapshotting
                | ReferenceApplyPhase::Creating
                | ReferenceApplyPhase::Verifying
                | ReferenceApplyPhase::Rebinding
                | ReferenceApplyPhase::Publishing
                | ReferenceApplyPhase::CleanupPending => {
                    let project_id = receipt.operation.project_id;
                    if let Err(error) = self.references.run_operation(self.as_ref(), receipt).await
                    {
                        tracing::error!(%error, "启动恢复失败，保留阻塞");
                    }
                    self.enqueue_superseding_references(project_id).await?;
                }
            }
        }
        self.launch_reference_operations().await?;
        let shutdown = self.references.shutdown_token();
        let weak = Arc::downgrade(self);
        self.references.spawn_worker(async move { loop {
            tokio::select! { _ = tokio::time::sleep(Duration::from_secs(1)) => {}, _ = shutdown.cancelled() => return }
            let Some(runtime) = weak.upgrade() else { return; };
            if let Err(error) = runtime.launch_reference_operations().await { tracing::error!(%error, "参考环境恢复循环失败"); }
        } });
        Ok(())
    }

    async fn launch_reference_operations(self: &Arc<Self>) -> Result<()> {
        for receipt in self.deps.store.reference_operations().await? {
            match receipt.operation.phase {
                ReferenceApplyPhase::Failed
                | ReferenceApplyPhase::Succeeded
                | ReferenceApplyPhase::Superseded => continue,
                ReferenceApplyPhase::WaitingIdle
                | ReferenceApplyPhase::Draining
                | ReferenceApplyPhase::Snapshotting
                | ReferenceApplyPhase::Creating
                | ReferenceApplyPhase::Verifying
                | ReferenceApplyPhase::Rebinding
                | ReferenceApplyPhase::Publishing
                | ReferenceApplyPhase::CleanupPending => {}
            }
            let Some(claim) = self.references.claim(receipt.operation.id) else {
                continue;
            };
            let runtime = Arc::clone(self);
            self.references.spawn_worker(async move {
                let _claim = claim;
                let project_id = receipt.operation.project_id;
                if let Err(error) = runtime
                    .references
                    .run_operation(runtime.as_ref(), receipt)
                    .await
                {
                    tracing::error!(%error, %project_id, "参考环境应用失败并保持阻塞");
                }
                if let Err(error) = runtime.enqueue_superseding_references(project_id).await {
                    tracing::warn!(%error, "新修订应用意图未能补偿");
                }
            });
        }
        Ok(())
    }

    async fn enqueue_superseding_references(&self, project_id: ProjectId) -> Result<()> {
        let current = self
            .deps
            .store
            .project_reference_configuration(project_id)
            .await?;
        let operations = self.deps.store.reference_operations().await?;
        let needs_next = operations.iter().any(|receipt| {
            receipt.operation.project_id == project_id
                && receipt.operation.target_revision < current.revision
                && matches!(
                    receipt.operation.phase,
                    ReferenceApplyPhase::Superseded
                        | ReferenceApplyPhase::Failed
                        | ReferenceApplyPhase::Succeeded
                )
        });
        if needs_next {
            let snapshot = self
                .references
                .catalog
                .snapshot(project_id, &current)
                .await?;
            self.deps
                .store
                .enqueue_reference_application(
                    project_id,
                    snapshot,
                    ReferenceApplyPolicy::WhenIdle,
                    mai_store::ReferenceApplyScope::Unapplied,
                )
                .await?;
        }
        Ok(())
    }
}

fn binding_state(
    binding: Option<&mai_store::PersistedReferenceBinding>,
    summary: &AgentSummary,
    target: u64,
) -> ReferenceApplicationState {
    match binding {
        Some(binding) if binding.applied_revision == target => ReferenceApplicationState::Applied,
        Some(_) => ReferenceApplicationState::Pending,
        None if summary.container_id.is_none() && target > 0 => ReferenceApplicationState::Deferred,
        None if summary.container_id.is_none() => ReferenceApplicationState::NoExecutionEnvironment,
        None if target == 0 => ReferenceApplicationState::Applied,
        None => ReferenceApplicationState::Pending,
    }
}
