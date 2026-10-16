//! 项目引用应用的领域入口；配置提交与 Turn gate 的线性化在同一个调用中完成。

use crate::admission::ReferenceAdmissions;
use crate::catalog::storage_error;
use crate::{
    ReferenceCatalog, ReferenceLeaseRegistry, Result, check_expected_revision, normalize_bindings,
};
use mai_docker::DockerClient;
use mai_protocol::*;
use mai_store::MaiStore;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use tokio::sync::{Semaphore, broadcast};
use uuid::Uuid;

pub struct ReferenceApplications {
    pub(crate) store: Arc<MaiStore>,
    pub(crate) docker: DockerClient,
    pub catalog: Arc<ReferenceCatalog>,
    pub admissions: ReferenceAdmissions,
    pub leases: ReferenceLeaseRegistry,
    pub(crate) concurrency: Semaphore,
    pub(crate) snapshot_resources: Arc<tokio::sync::RwLock<()>>,
    workers: tokio_util::task::TaskTracker,
    shutdown: tokio_util::sync::CancellationToken,
    projects: Mutex<HashMap<ProjectId, Arc<tokio::sync::Mutex<()>>>>,
    scheduled: Arc<Mutex<HashSet<Uuid>>>,
    pub(crate) observer: Option<Arc<dyn crate::ReferenceApplicationObserver>>,
    pub(crate) changes: broadcast::Sender<(ProjectId, AgentId, u64, ReferenceApplicationState)>,
}

impl ReferenceApplications {
    pub fn new(
        store: Arc<MaiStore>,
        docker: DockerClient,
        catalog: Arc<ReferenceCatalog>,
        observer: Option<Arc<dyn crate::ReferenceApplicationObserver>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            docker,
            catalog,
            admissions: ReferenceAdmissions::default(),
            leases: ReferenceLeaseRegistry::new(),
            concurrency: Semaphore::new(2),
            snapshot_resources: Arc::default(),
            workers: tokio_util::task::TaskTracker::new(),
            shutdown: tokio_util::sync::CancellationToken::new(),
            projects: Mutex::new(HashMap::new()),
            scheduled: Arc::default(),
            observer,
            changes: broadcast::channel(256).0,
        })
    }

    /// 后台任务归属应用服务；关闭后不再启动新的调度。
    pub fn spawn_worker(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        if !self.shutdown.is_cancelled() {
            self.workers.spawn(future);
        }
    }

    pub fn shutdown_token(&self) -> tokio_util::sync::CancellationToken {
        self.shutdown.clone()
    }

    /// 保留未完成收据并等待资源停止，避免线程已关闭后又被后台应用激活。
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        self.workers.close();
        self.workers.wait().await;
    }

    pub fn project_lock(&self, id: ProjectId) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.projects
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(id)
                .or_default(),
        )
    }

    pub fn subscribe(
        &self,
    ) -> broadcast::Receiver<(ProjectId, AgentId, u64, ReferenceApplicationState)> {
        self.changes.subscribe()
    }

    pub fn claim(&self, operation_id: Uuid) -> Option<ReferenceOperationClaim> {
        if !self
            .scheduled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(operation_id)
        {
            return None;
        }
        Some(ReferenceOperationClaim {
            operation_id,
            scheduled: Arc::clone(&self.scheduled),
        })
    }

    /// 临时 gate 原因先于 durable CAS；失败只释放自己的原因，成功转交给持久目标。
    pub async fn replace(
        &self,
        project_id: ProjectId,
        mut request: ReplaceProjectReferencesRequest,
    ) -> Result<ProjectReferenceConfiguration> {
        let project_lock = self.project_lock(project_id);
        let _project = project_lock.lock().await;
        let current = self
            .store
            .project_reference_configuration(project_id)
            .await
            .map_err(storage_error)?;
        check_expected_revision(request.expected_revision, current.revision)?;
        request.bindings = normalize_bindings(&request.bindings).into_bindings();
        let revision = if request.bindings == current.bindings {
            current.revision
        } else {
            crate::next_revision(current.revision)?
        };
        let target = ProjectReferenceConfiguration {
            revision,
            bindings: request.bindings.clone(),
        };
        let snapshot = self.catalog.snapshot(project_id, &target).await?;
        let inventory = self
            .store
            .reference_agent_inventory(project_id)
            .await
            .map_err(storage_error)?;
        let admissions = inventory
            .iter()
            .map(|(id, _)| self.admissions.agent(*id))
            .collect::<Vec<_>>();
        let guards = admissions
            .iter()
            .map(|entry| entry.gate.close())
            .collect::<Vec<_>>();
        let saved = self
            .store
            .replace_project_reference_configuration(project_id, request, snapshot)
            .await
            .map_err(storage_error)?;
        if saved.revision != current.revision {
            for ((_, physical), entry) in inventory.iter().zip(&admissions) {
                if *physical {
                    entry.block(saved.revision);
                }
            }
        }
        drop(guards);
        Ok(saved)
    }

    pub async fn request_apply(
        &self,
        project_id: ProjectId,
        request: ApplyProjectReferencesRequest,
    ) -> Result<()> {
        let project_lock = self.project_lock(project_id);
        let _project = project_lock.lock().await;
        let current = self
            .store
            .project_reference_configuration(project_id)
            .await
            .map_err(storage_error)?;
        check_expected_revision(request.expected_revision, current.revision)?;
        let snapshot = self.catalog.snapshot(project_id, &current).await?;
        let inventory = self
            .store
            .reference_agent_inventory(project_id)
            .await
            .map_err(storage_error)?;
        let admissions = inventory
            .iter()
            .map(|(id, _)| self.admissions.agent(*id))
            .collect::<Vec<_>>();
        let guards = admissions
            .iter()
            .map(|entry| entry.gate.close())
            .collect::<Vec<_>>();
        self.store
            .enqueue_reference_application(
                project_id,
                snapshot,
                request.policy,
                mai_store::ReferenceApplyScope::AllPhysicalEnvironments,
            )
            .await
            .map_err(storage_error)?;
        let operations = self
            .store
            .reference_operations()
            .await
            .map_err(storage_error)?;
        for operation in operations.into_iter().filter(|receipt| {
            receipt.operation.project_id == project_id
                && !matches!(
                    receipt.operation.phase,
                    ReferenceApplyPhase::Succeeded
                        | ReferenceApplyPhase::Failed
                        | ReferenceApplyPhase::Superseded
                )
        }) {
            self.admissions
                .agent(operation.operation.agent_id)
                .block(current.revision);
        }
        drop(guards);
        Ok(())
    }

    /// 启动屏障：任何 orphan 清理、Thread 恢复和自动审查之前重建全部未满足的 gate。
    pub async fn restore_admission(&self) -> Result<()> {
        let mut latest = HashMap::new();
        for receipt in self
            .store
            .reference_operations()
            .await
            .map_err(storage_error)?
        {
            latest.insert(receipt.operation.agent_id, receipt);
        }
        for receipt in latest.into_values() {
            match receipt.operation.phase {
                ReferenceApplyPhase::Succeeded | ReferenceApplyPhase::Superseded => {}
                ReferenceApplyPhase::WaitingIdle
                | ReferenceApplyPhase::Draining
                | ReferenceApplyPhase::Snapshotting
                | ReferenceApplyPhase::Creating
                | ReferenceApplyPhase::Verifying
                | ReferenceApplyPhase::Rebinding
                | ReferenceApplyPhase::Publishing
                | ReferenceApplyPhase::CleanupPending
                | ReferenceApplyPhase::Failed => {
                    self.admissions
                        .agent(receipt.operation.agent_id)
                        .block(receipt.operation.target_revision);
                }
            }
        }
        Ok(())
    }
}

pub struct ReferenceOperationClaim {
    operation_id: Uuid,
    scheduled: Arc<Mutex<HashSet<Uuid>>>,
}
impl Drop for ReferenceOperationClaim {
    fn drop(&mut self) {
        self.scheduled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.operation_id);
    }
}
