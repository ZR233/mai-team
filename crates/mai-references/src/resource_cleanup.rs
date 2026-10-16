//! 数据目录固定资源归属；快照创建、绑定发布与回收之间有显式读写屏障。
use crate::{
    ReferenceApplications, ReferenceError, Result, catalog::storage_error,
    environment::docker_error,
};
use mai_docker::{SnapshotCleanupReport, SnapshotRetention};
use mai_protocol::ReferenceErrorCode;
use mai_store::MaiStore;
use tokio::sync::OwnedRwLockReadGuard;
use uuid::Uuid;

/// 同一个规范数据库路径拥有稳定 UUID；不同实例不能互相回收未绑定镜像。
pub fn reference_snapshot_owner(store: &MaiStore) -> Result<Uuid> {
    let path = std::fs::canonicalize(store.path()).map_err(|error| {
        ReferenceError::new(
            ReferenceErrorCode::InternalError,
            "无法确定产品库的资源归属",
        )
        .with_cause(error)
    })?;
    let mut identity = b"mai.team.reference-snapshots.v1:".to_vec();
    identity.extend_from_slice(path.as_os_str().as_encoded_bytes());
    Ok(Uuid::new_v5(&Uuid::NAMESPACE_URL, &identity))
}

impl ReferenceApplications {
    /// 持有到快照绑定发布结束；回收不能观察到提交与持久化之间的空隙。
    pub async fn begin_snapshot_creation(&self) -> OwnedRwLockReadGuard<()> {
        self.snapshot_resources.clone().read_owned().await
    }

    /// 只回收当前产品库拥有、没有容器或恢复收据依赖的镜像。
    pub async fn reclaim_snapshot_resources(&self) -> Result<SnapshotCleanupReport> {
        let _resources = self.snapshot_resources.write().await;
        let owners = self
            .store
            .reference_resource_owners()
            .await
            .map_err(storage_error)?;
        let retention = SnapshotRetention::new()
            .with_image_ids(owners.image_ids)
            .with_operation_ids(owners.operation_ids)
            .with_clone_agent_ids(owners.agent_ids.into_iter().map(|id| id.to_string()));
        self.docker
            .reclaim_snapshot_images(&retention)
            .await
            .map_err(docker_error)
    }
}
