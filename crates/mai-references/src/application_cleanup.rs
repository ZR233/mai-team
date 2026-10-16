//! 新环境成功后收敛旧失败或已取代操作拥有的资源，再解除其源引用占用。
use crate::{ReferenceApplications, Result, catalog::storage_error, environment::docker_error};
use mai_docker::ContainerRemovalPolicy;
use mai_protocol::ReferenceApplyPhase;
use mai_store::PersistedReferenceOperation;

impl ReferenceApplications {
    pub(crate) async fn retire_previous_operations(
        &self,
        completed: &PersistedReferenceOperation,
        current_container: &str,
    ) -> Result<()> {
        for mut receipt in self
            .store
            .reference_operations()
            .await
            .map_err(storage_error)?
        {
            if receipt.operation.agent_id != completed.operation.agent_id
                || receipt.operation.id == completed.operation.id
                || !matches!(
                    receipt.operation.phase,
                    ReferenceApplyPhase::Failed | ReferenceApplyPhase::Superseded
                )
                || receipt.operation.created_at > completed.operation.created_at
            {
                continue;
            }
            let ids = [
                &receipt.operation.old_container_id,
                &receipt.operation.candidate_container_id,
            ]
            .into_iter()
            .flatten()
            // 元数据恢复会合法复用当前容器；失败收据不能删除已重新发布的身份。
            .filter(|id| id.as_str() != current_container)
            .cloned()
            .collect::<Vec<_>>();
            self.docker
                .remove_containers_exact(
                    &ids,
                    &[current_container.to_owned()],
                    ContainerRemovalPolicy::TolerateMissing,
                )
                .await
                .map_err(docker_error)?;
            receipt.operation.phase = ReferenceApplyPhase::Superseded;
            receipt.operation.updated_at = chrono::Utc::now();
            self.store
                .save_reference_operation(receipt)
                .await
                .map_err(storage_error)?;
        }
        Ok(())
    }
}
