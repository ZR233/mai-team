//! 应用状态机所需的产品回调；生命周期、资源和领域状态各有唯一所有者。

use crate::Result;
use mai_docker::ContainerHandle;
use mai_protocol::{AgentId, ReferenceApplyPolicy};
use mai_store::PersistedReferenceBinding;
use std::future::Future;

/// Runtime 只装配产品资源；实现保留 canonical Thread、排队输入和历史，
/// 在关闭的准入边界排空当前 Turn、替换工具，不能借换代关闭并取消下一次输入。
/// 所有回调都在该 Agent 生命周期锁内执行，不得递归获取同一把锁。
pub trait ReferenceEnvironmentHost: Send + Sync {
    fn quiesce(
        &self,
        agent_id: AgentId,
        policy: ReferenceApplyPolicy,
    ) -> impl Future<Output = Result<()>> + Send;
    fn rebind(
        &self,
        binding: PersistedReferenceBinding,
        container: ContainerHandle,
    ) -> impl Future<Output = Result<()>> + Send;
    fn activate(
        &self,
        binding: PersistedReferenceBinding,
    ) -> impl Future<Output = Result<()>> + Send;
}
