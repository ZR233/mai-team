//! 可重放的应用状态机；每个副作用之前先落阶段收据，发布前再次检查目标修订。

use crate::{
    AgentEnvironmentLease, ReferenceApplications, ReferenceEnvironmentHost, ReferenceError, Result,
};
use crate::{
    catalog::storage_error,
    environment::{docker_error, plan_from_environment},
};
use chrono::Utc;
use mai_docker::{
    ContainerEnvironmentFacts, ContainerHandle, ContainerRemovalPolicy, EnvironmentSnapshotRequest,
};
use mai_protocol::*;
use mai_store::{PersistedReferenceBinding, PersistedReferenceOperation};
use std::sync::Arc;
use tokio::time::{Duration, timeout};

impl ReferenceApplications {
    /// 同一 Agent 的恢复、新建、换代、删除共用生命周期锁；并发应用最多两个。
    pub async fn run_operation(
        &self,
        host: &impl ReferenceEnvironmentHost,
        mut receipt: PersistedReferenceOperation,
    ) -> Result<()> {
        let operation = &receipt.operation;
        let admission = self.admissions.agent(operation.agent_id);
        let _lifecycle = admission.lifecycle.lock().await;
        admission.block(operation.target_revision);
        let shutdown = self.shutdown_token();
        let result = tokio::select! {
            result = self.apply_inner(host, &mut receipt) => result,
            _ = shutdown.cancelled() => Err(ReferenceError::new(ReferenceErrorCode::EnvironmentBusy, "服务关闭，应用收据保留至下次启动恢复")),
        };
        if let Err(error) = &result {
            match timeout(
                Duration::from_secs(60),
                host.quiesce(
                    receipt.operation.agent_id,
                    ReferenceApplyPolicy::StopAndApply,
                ),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(close)) => {
                    tracing::error!(%close, "应用失败后关闭候选资源失败，保持准入关闭")
                }
                Err(close) => tracing::error!(%close, "应用失败后关闭候选资源超时，保持准入关闭"),
            }
            if let Err(cleanup) = self.leases.remove(receipt.operation.agent_id).await {
                tracing::error!(%cleanup, "旧 generation 清理未完成，保持阻塞");
            }
            for id in [
                &receipt.operation.candidate_container_id,
                &receipt.operation.old_container_id,
            ]
            .into_iter()
            .flatten()
            {
                if let Err(stop) = self.docker.stop_container(id).await {
                    tracing::error!(container_id = %id, %stop, "应用失败后停止容器失败，保持准入关闭");
                }
            }
            if shutdown.is_cancelled() {
                return result;
            }
            receipt.operation.failure = Some(crate::validation_failure(error, Utc::now()));
            self.phase(&mut receipt, ReferenceApplyPhase::Failed)
                .await?;
        }
        if result.is_ok()
            && let Err(error) = self.reclaim_snapshot_resources().await
        {
            tracing::warn!(%error, "快照回收未完成，保留资源等待后续重试");
        }
        result
    }

    async fn apply_inner(
        &self,
        host: &impl ReferenceEnvironmentHost,
        receipt: &mut PersistedReferenceOperation,
    ) -> Result<()> {
        let agent_id = receipt.operation.agent_id;
        let operation_id = receipt.operation.id;
        let admission = self.admissions.agent(agent_id);
        let current = self
            .store
            .project_reference_configuration(receipt.operation.project_id)
            .await
            .map_err(storage_error)?;
        // 只有尚无副作用的过期操作可以直接让位；有资源收据的操作先收敛再让位。
        if receipt.operation.target_revision != current.revision
            && receipt.operation.phase == ReferenceApplyPhase::WaitingIdle
        {
            self.phase(receipt, ReferenceApplyPhase::Superseded).await?;
            return Ok(());
        }
        let snapshot = Arc::new(
            self.catalog
                .snapshot(receipt.operation.project_id, &current)
                .await?,
        );
        if current.revision != receipt.operation.target_revision {
            return self.supersede(host, receipt).await;
        }
        if snapshot.fingerprint != receipt.target_snapshot.fingerprint {
            return Err(ReferenceError::new(
                ReferenceErrorCode::SourceIdentityChanged,
                "同一配置的源身份改变，请登记新快照并保存新修订",
            ));
        }
        // 说明等元数据变化不重建物理环境，也不打断当前 Turn。
        if let Some(mut binding) = self
            .store
            .reference_binding(agent_id)
            .await
            .map_err(storage_error)?
            && binding.fingerprint == snapshot.fingerprint
            && receipt.operation.phase == ReferenceApplyPhase::WaitingIdle
        {
            let facts = self
                .docker
                .inspect_container_environment(&binding.container_id)
                .await
                .map_err(docker_error)?;
            let plan = plan_from_environment(
                agent_id,
                facts.generation.map(|_| binding.generation),
                &binding.effective_image_id,
                &snapshot,
                &facts,
            )?;
            let verified = self
                .docker
                .verify_environment(&plan, &binding.container_id)
                .await;
            if let Ok(verified) = verified {
                let resident_lease = self
                    .leases
                    .acquire_generation(agent_id, binding.generation)
                    .await
                    .is_ok();
                if !resident_lease {
                    let _capacity = self.concurrency.acquire().await.map_err(|error| {
                        ReferenceError::new(
                            ReferenceErrorCode::EnvironmentApplyFailed,
                            "应用调度已关闭",
                        )
                        .with_cause(error)
                    })?;
                    timeout(
                        Duration::from_secs(60),
                        host.quiesce(agent_id, receipt.operation.policy),
                    )
                    .await
                    .map_err(|error| {
                        ReferenceError::new(
                            ReferenceErrorCode::EnvironmentBusy,
                            "恢复相同环境时关闭线程超时",
                        )
                        .with_cause(error)
                    })??;
                    let container = self
                        .docker
                        .prepare_verified_container(&verified)
                        .await
                        .map_err(docker_error)?;
                    self.leases
                        .publish(AgentEnvironmentLease::new(
                            agent_id,
                            &binding.container_id,
                            binding.generation,
                            Arc::clone(&snapshot),
                        ))
                        .await?;
                    host.rebind(binding.clone(), container).await?;
                }
                binding.applied_revision = current.revision;
                binding.snapshot = (*snapshot).clone();
                binding.updated_at = Utc::now();
                self.store
                    .publish_reference_binding(binding.clone())
                    .await
                    .map_err(storage_error)?;
                self.leases
                    .publish(AgentEnvironmentLease::new(
                        agent_id,
                        &binding.container_id,
                        binding.generation,
                        Arc::clone(&snapshot),
                    ))
                    .await?;
                if !resident_lease {
                    host.activate(binding.clone()).await?;
                }
                self.retire_previous_operations(receipt, &binding.container_id)
                    .await?;
                self.phase(receipt, ReferenceApplyPhase::Succeeded).await?;
                admission.published(current.revision);
                return Ok(());
            }
            // 配置指纹相同并不代表实际环境匹配。漂移仍走完整换代，不能永久卡在同修订。
        }
        if receipt.operation.phase == ReferenceApplyPhase::WaitingIdle {
            loop {
                // StopAndApply 可由同一目标的后续请求升级；等待期间不持有应用并发额度。
                if let Some(latest) = self
                    .store
                    .reference_operations()
                    .await
                    .map_err(storage_error)?
                    .into_iter()
                    .find(|item| item.operation.id == operation_id)
                {
                    receipt.operation.policy = latest.operation.policy;
                }
                if receipt.operation.policy == ReferenceApplyPolicy::StopAndApply
                    || admission.gate.admitted_turns() == 0
                {
                    break;
                }
                if self
                    .store
                    .project_reference_configuration(receipt.operation.project_id)
                    .await
                    .map_err(storage_error)?
                    .revision
                    != receipt.operation.target_revision
                {
                    self.phase(receipt, ReferenceApplyPhase::Superseded).await?;
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        let _capacity = self.concurrency.acquire().await.map_err(|error| {
            ReferenceError::new(ReferenceErrorCode::EnvironmentApplyFailed, "应用调度已关闭")
                .with_cause(error)
        })?;
        self.phase(receipt, ReferenceApplyPhase::Draining).await?;
        timeout(
            Duration::from_secs(60),
            host.quiesce(agent_id, receipt.operation.policy),
        )
        .await
        .map_err(|error| {
            ReferenceError::new(
                ReferenceErrorCode::EnvironmentBusy,
                "等待当前 Turn 和环境资源排空超过 60 秒，环境保持阻塞",
            )
            .with_cause(error)
        })??;
        self.leases.remove(agent_id).await?;
        let old_id = receipt.operation.old_container_id.clone().ok_or_else(|| {
            ReferenceError::new(
                ReferenceErrorCode::EnvironmentRecoveryUnavailable,
                "操作没有旧容器收据，拒绝退回基础镜像",
            )
        })?;
        let facts: ContainerEnvironmentFacts = match &receipt.old_mounts {
            Some(saved) => serde_json::from_value(saved.clone()).map_err(|error| {
                ReferenceError::new(
                    ReferenceErrorCode::EnvironmentRecoveryUnavailable,
                    "旧环境卷收据损坏",
                )
                .with_cause(error)
            })?,
            None => self
                .docker
                .inspect_container_environment(&old_id)
                .await
                .map_err(|error| match error {
                    mai_docker::DockerError::UnknownContainer(id) => ReferenceError::new(
                        ReferenceErrorCode::EnvironmentRecoveryUnavailable,
                        format!("旧容器 {id} 缺失且操作没有已保存的卷身份；需要恢复成套备份，拒绝退回基础镜像"),
                    ),
                    error => docker_error(error),
                })?,
        };
        receipt.old_mounts = Some(serde_json::to_value(&facts).map_err(|error| {
            ReferenceError::new(ReferenceErrorCode::InternalError, "无法保存实际卷身份")
                .with_cause(error)
        })?);
        let _snapshot_resources = self.begin_snapshot_creation().await;
        self.phase(receipt, ReferenceApplyPhase::Snapshotting)
            .await?;
        let saved_image = self
            .docker
            .snapshot_receipt(operation_id)
            .await
            .map_err(docker_error)?;
        let image = match saved_image {
            Some(saved) => saved.image_id,
            None => {
                self.docker
                    .stop_container(&old_id)
                    .await
                    .map_err(docker_error)?;
                self.docker
                    .commit_stopped_environment(
                        &EnvironmentSnapshotRequest::new(
                            operation_id,
                            agent_id.to_string(),
                            &old_id,
                        )
                        .map_err(docker_error)?,
                    )
                    .await
                    .map_err(docker_error)?
                    .image_id
            }
        };
        receipt.operation.snapshot_image_id = Some(image.clone());
        let plan = plan_from_environment(agent_id, Some(operation_id), &image, &snapshot, &facts)?;
        self.phase(receipt, ReferenceApplyPhase::Creating).await?;
        // 创建成功、ID 未落盘时崩溃：按确定 generation 找回，不能另造一个候选。
        let candidate = self
            .docker
            .list_managed_containers()
            .await
            .map_err(docker_error)?
            .into_iter()
            .find(|item| {
                !item.sidecar
                    && item.agent_id.as_deref() == Some(agent_id.to_string().as_str())
                    && item.generation == Some(operation_id)
            });
        let container = match candidate {
            Some(candidate) => ContainerHandle {
                id: candidate.id,
                name: candidate.name,
                image: image.clone(),
            },
            None => self
                .docker
                .create_environment(&plan, &[])
                .await
                .map_err(docker_error)?,
        };
        receipt.operation.candidate_container_id = Some(container.id.clone());
        self.phase(receipt, ReferenceApplyPhase::Verifying).await?;
        let verified = self
            .docker
            .verify_environment(&plan, &container.id)
            .await
            .map_err(docker_error)?;
        self.docker
            .prepare_verified_container(&verified)
            .await
            .map_err(docker_error)?;
        let binding = PersistedReferenceBinding {
            agent_id,
            project_id: Some(snapshot.project_id),
            generation: operation_id,
            container_id: container.id.clone(),
            effective_image_id: image,
            applied_revision: snapshot.configuration_revision,
            fingerprint: snapshot.fingerprint.clone(),
            snapshot: (*snapshot).clone(),
            updated_at: Utc::now(),
        };
        self.phase(receipt, ReferenceApplyPhase::Rebinding).await?;
        // 此租约仅供被 gate 阻挡的资源装配使用；发布 fence 失败会撤销并停止候选。
        self.leases
            .publish(AgentEnvironmentLease::new(
                agent_id,
                &container.id,
                operation_id,
                Arc::clone(&snapshot),
            ))
            .await?;
        host.rebind(binding.clone(), container).await?;
        self.phase(receipt, ReferenceApplyPhase::Publishing).await?;
        let project_lock = self.project_lock(snapshot.project_id);
        let publication = project_lock.lock().await;
        let current = self
            .store
            .project_reference_configuration(snapshot.project_id)
            .await
            .map_err(storage_error)?;
        let verified_snapshot = self.catalog.snapshot(snapshot.project_id, &current).await?;
        if verified_snapshot.configuration_revision != snapshot.configuration_revision
            || verified_snapshot.fingerprint != snapshot.fingerprint
        {
            drop(publication);
            return self.supersede(host, receipt).await;
        }
        self.store
            .publish_reference_binding(binding.clone())
            .await
            .map_err(storage_error)?;
        drop(publication);
        host.activate(binding.clone()).await?;
        self.phase(receipt, ReferenceApplyPhase::CleanupPending)
            .await?;
        self.docker
            .remove_containers_exact(
                &[old_id],
                std::slice::from_ref(&binding.container_id),
                ContainerRemovalPolicy::TolerateMissing,
            )
            .await
            .map_err(docker_error)?;
        self.retire_previous_operations(receipt, &binding.container_id)
            .await?;
        self.phase(receipt, ReferenceApplyPhase::Succeeded).await?;
        admission.published(snapshot.configuration_revision);
        Ok(())
    }

    async fn supersede(
        &self,
        host: &impl ReferenceEnvironmentHost,
        receipt: &mut PersistedReferenceOperation,
    ) -> Result<()> {
        timeout(
            Duration::from_secs(60),
            host.quiesce(
                receipt.operation.agent_id,
                ReferenceApplyPolicy::StopAndApply,
            ),
        )
        .await
        .map_err(|error| {
            ReferenceError::new(
                ReferenceErrorCode::EnvironmentBusy,
                "取消过期候选时关闭资源超时",
            )
            .with_cause(error)
        })??;
        self.leases.remove(receipt.operation.agent_id).await?;
        if let Some(id) = &receipt.operation.candidate_container_id {
            self.docker.stop_container(id).await.map_err(docker_error)?;
            let published = self
                .store
                .reference_binding(receipt.operation.agent_id)
                .await
                .map_err(storage_error)?;
            // DB 发布先于 activation。此窗口的新修订必须从已发布容器保存可写层；
            // 它虽然不再允许执行，仍是下一次换代的受保护旧环境。
            if published
                .as_ref()
                .is_none_or(|binding| binding.container_id != *id)
            {
                self.docker
                    .remove_containers_exact(
                        std::slice::from_ref(id),
                        &[],
                        ContainerRemovalPolicy::TolerateMissing,
                    )
                    .await
                    .map_err(docker_error)?;
            }
        }
        self.phase(receipt, ReferenceApplyPhase::Superseded).await
    }

    async fn phase(
        &self,
        receipt: &mut PersistedReferenceOperation,
        phase: ReferenceApplyPhase,
    ) -> Result<()> {
        receipt.operation.phase = phase;
        receipt.operation.updated_at = Utc::now();
        self.store
            .save_reference_operation(receipt.clone())
            .await
            .map_err(storage_error)?;
        if let Some(observer) = &self.observer {
            observer.recorded(receipt);
        }
        let state = match phase {
            ReferenceApplyPhase::WaitingIdle => ReferenceApplicationState::WaitingIdle,
            ReferenceApplyPhase::Succeeded => ReferenceApplicationState::Applied,
            ReferenceApplyPhase::Failed => ReferenceApplicationState::Failed,
            ReferenceApplyPhase::Superseded => ReferenceApplicationState::Pending,
            ReferenceApplyPhase::Draining
            | ReferenceApplyPhase::Snapshotting
            | ReferenceApplyPhase::Creating
            | ReferenceApplyPhase::Verifying
            | ReferenceApplyPhase::Rebinding
            | ReferenceApplyPhase::Publishing
            | ReferenceApplyPhase::CleanupPending => ReferenceApplicationState::Applying,
        };
        let _ = self.changes.send((
            receipt.operation.project_id,
            receipt.operation.agent_id,
            receipt.operation.target_revision,
            state,
        ));
        Ok(())
    }
}

#[cfg(test)]
#[path = "application_supersede_tests.rs"]
mod tests;
