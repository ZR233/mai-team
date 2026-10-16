//! 固定环境租约：一个 turn 与其全部工具调用绑定同一个容器、同一份参考快照，以及同一个
//! generation 异步资源生命周期。
//!
//! 约束：
//! - 每个 Agent 在任意时刻至多一个有效 generation；发布新 generation 会先撤销旧 generation 并
//!   等它登记的后台资源真实收敛，然后才安装新租约。因此残存句柄会收到 `environment_superseded`，
//!   既不会被悄悄切到新容器，也不会留下"旧容器仍在写、应用已经提交"的窗口。
//! - 同一个 generation 只替换 metadata（名称、说明、快照）时继承原有的资源生命周期：旧 Turn
//!   继续用自己冻结的快照，它登记的任务仍归属同一个 generation。
//! - 本模块不接触 Docker 的具体调用，只保存"已经验证过的物理身份"和该 generation 的异步资源
//!   事实；sidecar、CLI 进程这类资源的删除与等待由上层登记进来（见 `lifecycle` 子模块）。

mod lifecycle;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use mai_protocol::{AgentId, AppliedReferenceSnapshot, ReferenceErrorCode};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::{ReferenceError, Result};

pub use lifecycle::{GenerationLifecycle, TrackedTask};

/// 某个 Agent 在一个 generation 上生效的环境租约。
#[derive(Debug)]
pub struct AgentEnvironmentLease {
    agent_id: AgentId,
    container_id: String,
    generation: Uuid,
    references: Arc<AppliedReferenceSnapshot>,
    lifecycle: Arc<GenerationLifecycle>,
}

impl AgentEnvironmentLease {
    /// 用已经验证过的容器与参考快照构造租约，并为这个 generation 新建空的资源生命周期。
    pub fn new(
        agent_id: AgentId,
        container_id: impl Into<String>,
        generation: Uuid,
        references: Arc<AppliedReferenceSnapshot>,
    ) -> Self {
        Self {
            agent_id,
            container_id: container_id.into(),
            generation,
            references,
            lifecycle: Arc::new(GenerationLifecycle::new(generation)),
        }
    }

    /// 租约所属 Agent。
    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    /// 固定的容器 ID；同一个 turn 内不允许重新按 agent 查容器。
    pub fn container_id(&self) -> &str {
        &self.container_id
    }

    /// 环境 generation。
    pub fn generation(&self) -> Uuid {
        self.generation
    }

    /// 本次 generation 生效的参考快照。
    pub fn references(&self) -> &AppliedReferenceSnapshot {
        &self.references
    }

    /// 参考快照的共享句柄。
    pub fn shared_references(&self) -> Arc<AppliedReferenceSnapshot> {
        Arc::clone(&self.references)
    }

    /// 本 generation 的异步资源生命周期：登记后台任务、撤销与等待真实收敛都走它。
    pub fn lifecycle(&self) -> &Arc<GenerationLifecycle> {
        &self.lifecycle
    }

    /// 该 generation 的取消令牌；换代会触发取消。
    pub fn cancellation(&self) -> CancellationToken {
        self.lifecycle.cancellation()
    }

    /// 登记一段必须等到真实结束的后台工作（sidecar、CLI 进程、后台命令）。
    ///
    /// # Errors
    /// 租约已撤销（或撤销正在开始）时返回 `environment_superseded`：撤销 fence 落下之后不再
    /// 接受新登记，因此"撤销 + 等待"不会漏掉后来者。
    pub fn spawn_tracked<F>(&self, future: F) -> Result<TrackedTask<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.lifecycle.spawn_tracked(future)
    }

    /// 换用同一个 generation 已有的那一份资源生命周期。
    ///
    /// 只有"同一个 generation 只换 metadata"的发布路径会调用它：旧句柄保持自己的快照，双方
    /// 登记的任务仍归属同一个 generation，撤销时一起被等到。
    pub(crate) fn adopt_lifecycle(&mut self, lifecycle: Arc<GenerationLifecycle>) {
        self.lifecycle = lifecycle;
    }

    /// 租约是否已被撤销。
    pub fn is_revoked(&self) -> bool {
        self.lifecycle.is_revoked()
    }

    /// 同步撤销租约；重复调用无副作用。
    ///
    /// 它只做 fence 与取消，**不等**登记的后台资源真实结束；要等必须继续 await
    /// [`Self::revoke_and_drain`]。
    pub fn revoke(&self) {
        self.lifecycle.revoke();
    }

    /// 撤销并等全部已登记的后台资源真实结束。
    ///
    /// # Errors
    /// 期间记载了真实清理失败（例如 sidecar 删除失败）时返回该失败：调用方必须 fail closed，
    /// 不得据此提交或发布候选环境。
    pub async fn revoke_and_drain(&self) -> Result<()> {
        self.lifecycle.revoke();
        self.lifecycle.drain().await
    }

    /// 准入检查：已撤销的租约必须失败，不允许自动改用新容器。
    pub fn ensure_active(&self) -> Result<()> {
        if self.is_revoked() {
            return Err(ReferenceError::new(
                ReferenceErrorCode::EnvironmentSuperseded,
                format!(
                    "Agent {} 的环境 generation {} 已被撤销，请重新取得当前环境",
                    self.agent_id, self.generation
                ),
            ));
        }
        Ok(())
    }
}

/// 发布结果：新租约与本次被取代的 generation（若与旧租约不同）。
#[derive(Debug)]
pub struct LeasePublication {
    lease: Arc<AgentEnvironmentLease>,
    superseded_generation: Option<Uuid>,
}

impl LeasePublication {
    /// 新发布的租约。
    pub fn lease(&self) -> &Arc<AgentEnvironmentLease> {
        &self.lease
    }

    /// 被取代的旧 generation；首次发布或重复发布同一 generation 时为 `None`。
    pub fn superseded_generation(&self) -> Option<Uuid> {
        self.superseded_generation
    }

    /// 取出租约句柄。
    pub fn into_lease(self) -> Arc<AgentEnvironmentLease> {
        self.lease
    }
}

/// 每个 Agent 至多一个当前有效 generation 的租约登记表。
///
/// 登记表本身只在短临界区里读写；`publish`/`remove`/`revoke` 之间用每个 Agent 一把的"变更锁"
/// 串行化，因此等待真实收敛的长 await 既不阻塞其它 Agent，也不会让并发换代互相覆盖。
/// 撤销或清理失败的 generation 会**留在登记表里**（已撤销、`acquire` 即失败），直到某次 drain
/// 成功才被替换或摘除；任何错误都不删除 fail closed 的事实。记载过真实清理失败的 generation 是
/// 粘性的：它不会因为再次等待而变干净，也不提供自动清除，只能靠服务恢复（进程重启）或人工确认。
#[derive(Debug, Default)]
pub struct ReferenceLeaseRegistry {
    leases: RwLock<BTreeMap<AgentId, Arc<AgentEnvironmentLease>>>,
    /// 每个 Agent 一把变更锁；只随 Agent 数增长，不随租约换代增长。
    changes: RwLock<BTreeMap<AgentId, Arc<Mutex<()>>>>,
}

impl ReferenceLeaseRegistry {
    /// 空登记表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 取得该 Agent 的变更锁；没有就登记一把。
    async fn change_gate(&self, agent_id: AgentId) -> Arc<Mutex<()>> {
        if let Some(gate) = self.changes.read().await.get(&agent_id) {
            return Arc::clone(gate);
        }
        let mut changes = self.changes.write().await;
        Arc::clone(
            changes
                .entry(agent_id)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// 发布一个租约。
    ///
    /// 同一个 generation 只替换 metadata：新租约继承旧租约的资源生命周期，旧句柄保持自己冻结
    /// 的参考快照，双方登记的任务仍归属同一个 generation，因此**不**触发撤销或等待。同一
    /// generation 的发布必须保持物理身份（容器）完全一致，且旧生命周期没有被撤销：generation
    /// 标识的就是那个被验证过的环境，换容器或复活已撤销的 generation 都必须被拒绝。
    ///
    /// 不同 generation 时会先同步撤销旧 generation（fence 掉它的后续登记），再在锁外等它登记的
    /// 后台资源真实收敛，**成功之后才**安装新 generation；失败时旧 generation 留在登记表里
    /// （已撤销、`acquire` 即 `environment_superseded`），重试仍会看到同一个失败。
    ///
    /// 同一个 Agent 的 `publish`/`remove`/`revoke` 之间由该 Agent 的变更锁串行化，因此并发换代
    /// 不会把已撤销的 generation 覆盖回可用状态，也不会漏掉它的撤销等待。
    ///
    /// # Errors
    /// 旧 generation 的真实清理失败时返回该失败（粘性），并且**不**安装新 generation：调用方必须
    /// fail closed，不得据此提交或发布候选环境，重试也无法绕过。同一 generation 的发布遇到
    /// 已撤销的生命周期返回 `environment_superseded`，遇到不同的容器返回
    /// `environment_apply_failed`。
    pub async fn publish(&self, mut lease: AgentEnvironmentLease) -> Result<LeasePublication> {
        let agent_id = lease.agent_id();
        let change = self.change_gate(agent_id).await;
        let _change = change.lock().await;

        let current = self.leases.read().await.get(&agent_id).map(Arc::clone);
        let Some(previous) = current else {
            let lease = Arc::new(lease);
            self.leases
                .write()
                .await
                .insert(agent_id, Arc::clone(&lease));
            return Ok(LeasePublication {
                lease,
                superseded_generation: None,
            });
        };
        if previous.generation() == lease.generation() {
            if previous.is_revoked() {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::EnvironmentSuperseded,
                    format!(
                        "Agent {agent_id} 的 generation {} 已被撤销，不能重新发布成可用环境；请用新的 generation 重新应用",
                        lease.generation()
                    ),
                ));
            }
            if previous.container_id() != lease.container_id() {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::EnvironmentApplyFailed,
                    format!(
                        "Agent {agent_id} 的 generation {} 已经绑定容器 {}，不能改成 {}；换容器属于新 generation",
                        lease.generation(),
                        previous.container_id(),
                        lease.container_id()
                    ),
                ));
            }
            lease.adopt_lifecycle(Arc::clone(previous.lifecycle()));
            let lease = Arc::new(lease);
            self.leases
                .write()
                .await
                .insert(agent_id, Arc::clone(&lease));
            return Ok(LeasePublication {
                lease,
                superseded_generation: None,
            });
        }
        // 不同 generation：撤销 + 等真实收敛（reconcile 的第一步就是同步 fence，旧代留在登记表里，
        // 因此等待期间 acquire 会拿到 typed superseded）。
        let superseded_generation = previous.generation();
        previous.revoke_and_drain().await?;
        let lease = Arc::new(lease);
        self.leases
            .write()
            .await
            .insert(agent_id, Arc::clone(&lease));
        Ok(LeasePublication {
            lease,
            superseded_generation: Some(superseded_generation),
        })
    }

    /// 取得该 Agent 当前有效租约；没有租约或租约已撤销时失败。
    pub async fn acquire(&self, agent_id: AgentId) -> Result<Arc<AgentEnvironmentLease>> {
        let lease = {
            let leases = self.leases.read().await;
            leases.get(&agent_id).map(Arc::clone)
        };
        match lease {
            None => Err(ReferenceError::new(
                ReferenceErrorCode::EnvironmentNotReady,
                format!("Agent {agent_id} 还没有已发布且验证通过的参考环境"),
            )),
            Some(lease) => {
                lease.ensure_active()?;
                Ok(lease)
            }
        }
    }

    /// 取得指定 generation 的租约；generation 不匹配视为已被取代。
    pub async fn acquire_generation(
        &self,
        agent_id: AgentId,
        generation: Uuid,
    ) -> Result<Arc<AgentEnvironmentLease>> {
        let lease = self.acquire(agent_id).await?;
        if lease.generation() != generation {
            return Err(ReferenceError::new(
                ReferenceErrorCode::EnvironmentSuperseded,
                format!(
                    "Agent {agent_id} 的当前 generation 是 {}，请求的 {generation} 已被取代",
                    lease.generation()
                ),
            ));
        }
        Ok(lease)
    }

    /// 撤销指定 generation 的租约，并等它登记的后台资源真实收敛；generation 不匹配时不动作。
    ///
    /// # Errors
    /// 真实清理失败时返回该失败（粘性）：调用方必须 fail closed。租约保持"已撤销"并留在登记表里
    /// （`acquire` 即 `environment_superseded`），不会因为失败而被重新启用，也不会被遗忘——重试
    /// `revoke`/`remove`/`publish` 都会再看到同一个失败。
    pub async fn revoke(&self, agent_id: AgentId, generation: Uuid) -> Result<bool> {
        let change = self.change_gate(agent_id).await;
        let _change = change.lock().await;

        let current = self.leases.read().await.get(&agent_id).map(Arc::clone);
        let Some(previous) = current else {
            return Ok(false);
        };
        if previous.generation() != generation {
            return Ok(false);
        }
        previous.revoke_and_drain().await?;
        self.leases.write().await.remove(&agent_id);
        Ok(true)
    }

    /// 移除该 Agent 的租约、撤销它，并等它登记的后台资源真实收敛。
    ///
    /// 返回值是本次被移除的 generation（若原本没有登记则为 `None`）。
    ///
    /// # Errors
    /// 真实清理失败时返回该失败（粘性）：调用方必须 fail closed，不得在失败之后提交或发布候选
    /// 环境。失败时租约**留在**登记表里（已撤销），因此重试仍会看到同一个失败，不会因为登记表变空
    /// 而绕过。
    pub async fn remove(&self, agent_id: AgentId) -> Result<Option<Uuid>> {
        let change = self.change_gate(agent_id).await;
        let _change = change.lock().await;

        let current = self.leases.read().await.get(&agent_id).map(Arc::clone);
        let Some(previous) = current else {
            return Ok(None);
        };
        previous.revoke_and_drain().await?;
        self.leases.write().await.remove(&agent_id);
        Ok(Some(previous.generation()))
    }

    /// 该 Agent 当前登记的 generation。
    pub async fn current_generation(&self, agent_id: AgentId) -> Option<Uuid> {
        let leases = self.leases.read().await;
        leases.get(&agent_id).map(|lease| lease.generation())
    }

    /// 当前有租约登记的 Agent 列表。
    pub async fn tracked(&self) -> Vec<AgentId> {
        let leases = self.leases.read().await;
        leases.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;
    use chrono::Utc;
    use pretty_assertions::assert_eq;
    use tokio::sync::oneshot;

    fn snapshot() -> Arc<AppliedReferenceSnapshot> {
        metadata_snapshot(1)
    }

    /// 同一个 generation 换 metadata 时的快照：revision 变了，挂载指纹不变。
    fn metadata_snapshot(configuration_revision: u64) -> Arc<AppliedReferenceSnapshot> {
        Arc::new(AppliedReferenceSnapshot {
            project_id: Uuid::new_v4(),
            configuration_revision,
            fingerprint: "sha256:fingerprint".to_string(),
            references: Vec::new(),
            observed_at: Utc::now(),
        })
    }

    #[tokio::test]
    async fn publishing_a_new_generation_revokes_the_old_lease() {
        let registry = ReferenceLeaseRegistry::new();
        let agent_id = Uuid::new_v4();
        let first_generation = Uuid::new_v4();
        let second_generation = Uuid::new_v4();

        let publication = registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-1",
                first_generation,
                snapshot(),
            ))
            .await
            .expect("发布首个 generation");
        assert_eq!(None, publication.superseded_generation());
        let first = publication.into_lease();
        assert!(first.ensure_active().is_ok());
        assert_eq!("container-1", first.container_id());

        let publication = registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-2",
                second_generation,
                snapshot(),
            ))
            .await
            .expect("发布第二个 generation：没有登记资源需要等待，收敛是干净的");
        assert_eq!(Some(first_generation), publication.superseded_generation());

        // 残存句柄必须失败，且不能自动指向新容器。
        assert_eq!(
            ReferenceErrorCode::EnvironmentSuperseded,
            first.ensure_active().expect_err("旧租约已被撤销").code()
        );
        assert!(first.is_revoked());
        let current = registry.acquire(agent_id).await.expect("当前租约");
        assert_eq!("container-2", current.container_id());
        assert_eq!(second_generation, current.generation());
    }

    #[tokio::test]
    async fn acquire_without_lease_is_not_ready() {
        let registry = ReferenceLeaseRegistry::new();
        let error = registry
            .acquire(Uuid::new_v4())
            .await
            .expect_err("没有租约");
        assert_eq!(ReferenceErrorCode::EnvironmentNotReady, error.code());
        assert_eq!(409, error.http_status());
    }

    #[tokio::test]
    async fn acquire_generation_rejects_superseded_generation() {
        let registry = ReferenceLeaseRegistry::new();
        let agent_id = Uuid::new_v4();
        let generation = Uuid::new_v4();
        registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-1",
                generation,
                snapshot(),
            ))
            .await
            .expect("发布租约");

        assert!(
            registry
                .acquire_generation(agent_id, generation)
                .await
                .is_ok()
        );
        assert_eq!(
            ReferenceErrorCode::EnvironmentSuperseded,
            registry
                .acquire_generation(agent_id, Uuid::new_v4())
                .await
                .expect_err("generation 不匹配")
                .code()
        );
    }

    #[tokio::test]
    async fn revoke_and_remove_keep_the_registry_consistent() {
        let registry = ReferenceLeaseRegistry::new();
        let agent_id = Uuid::new_v4();
        let generation = Uuid::new_v4();
        registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-1",
                generation,
                snapshot(),
            ))
            .await
            .expect("发布租约");

        assert_eq!(vec![agent_id], registry.tracked().await);
        assert_eq!(
            Some(generation),
            registry.current_generation(agent_id).await
        );
        assert!(
            !registry
                .revoke(agent_id, Uuid::new_v4())
                .await
                .expect("generation 不匹配时不动作")
        );
        assert!(
            registry
                .revoke(agent_id, generation)
                .await
                .expect("撤销当前 generation")
        );
        assert!(registry.acquire(agent_id).await.is_err());

        let generation = Uuid::new_v4();
        registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-3",
                generation,
                snapshot(),
            ))
            .await
            .expect("发布新 generation");
        assert_eq!(
            Some(generation),
            registry.remove(agent_id).await.expect("移除租约")
        );
        assert!(registry.tracked().await.is_empty());
        assert_eq!(None, registry.remove(agent_id).await.expect("没有租约"));
    }

    #[tokio::test]
    async fn replacing_metadata_in_the_same_generation_keeps_snapshot_and_waits_for_tasks() {
        let registry = ReferenceLeaseRegistry::new();
        let agent_id = Uuid::new_v4();
        let generation = Uuid::new_v4();
        let first = registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-1",
                generation,
                snapshot(),
            ))
            .await
            .expect("首次发布")
            .into_lease();

        // 旧 Turn 登记的"正在跑的资源"：只有测试显式释放它才会结束。
        let (release, wait_for_release) = oneshot::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let task_finished = Arc::clone(&finished);
        let mut tracked = first
            .spawn_tracked(async move {
                let _ = wait_for_release.await;
                task_finished.store(true, Ordering::SeqCst);
            })
            .expect("撤销前登记后台资源");

        let publication = registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-1",
                generation,
                metadata_snapshot(2),
            ))
            .await
            .expect("同 generation 只换 metadata");
        assert_eq!(None, publication.superseded_generation());
        let second = publication.into_lease();

        // 同 generation：不撤销、不等，句柄共享同一个资源生命周期；旧句柄保持自己冻结的快照。
        assert!(!first.is_revoked());
        assert!(Arc::ptr_eq(first.lifecycle(), second.lifecycle()));
        assert_eq!(1, first.references().configuration_revision);
        assert_eq!(2, second.references().configuration_revision);

        // 撤销时必须等到旧句柄登记的任务真实结束：任务还阻塞时不能声称已经收敛。
        first.revoke();
        let early =
            tokio::time::timeout(Duration::from_millis(50), second.revoke_and_drain()).await;
        assert!(early.is_err(), "后台资源还在跑时不能声称已经收敛");
        assert!(!finished.load(Ordering::SeqCst));

        release.send(()).expect("释放后台资源");
        second.revoke_and_drain().await.expect("资源结束后收敛干净");
        assert!(finished.load(Ordering::SeqCst));
        tracked.wait().await.expect("已登记任务的句柄仍可等待");
    }

    #[tokio::test]
    async fn registration_is_refused_after_revoke() {
        let lease =
            AgentEnvironmentLease::new(Uuid::new_v4(), "container-1", Uuid::new_v4(), snapshot());
        let mut registered = lease
            .spawn_tracked(async {})
            .expect("撤销前可以登记后台资源");

        lease.revoke();
        let error = lease
            .spawn_tracked(async {})
            .expect_err("撤销 fence 落下后必须拒绝新登记");
        assert_eq!(ReferenceErrorCode::EnvironmentSuperseded, error.code());

        // 撤销之前登记的任务仍属同一个 generation，会被等到。
        registered.wait().await.expect("已登记任务正常结束");
        lease.revoke_and_drain().await.expect("收敛干净");
    }

    #[tokio::test]
    async fn remove_waits_for_registered_resources_to_finish() {
        let registry = Arc::new(ReferenceLeaseRegistry::new());
        let agent_id = Uuid::new_v4();
        let generation = Uuid::new_v4();
        let lease = registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-1",
                generation,
                snapshot(),
            ))
            .await
            .expect("发布租约")
            .into_lease();

        let (release, wait_for_release) = oneshot::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let task_finished = Arc::clone(&finished);
        let mut tracked = lease
            .spawn_tracked(async move {
                let _ = wait_for_release.await;
                task_finished.store(true, Ordering::SeqCst);
            })
            .expect("登记后台资源");

        let registry_for_remove = Arc::clone(&registry);
        let mut remover = tokio::spawn(async move { registry_for_remove.remove(agent_id).await });
        let early = tokio::time::timeout(Duration::from_millis(50), &mut remover).await;
        assert!(
            early.is_err(),
            "真实资源还没结束，remove 不能返回并放行后续提交"
        );

        release.send(()).expect("释放后台资源");
        let removed = remover.await.expect("remove 任务结束").expect("移除租约");
        assert_eq!(Some(generation), removed);
        assert!(finished.load(Ordering::SeqCst));
        assert!(registry.tracked().await.is_empty());
        tracked.wait().await.expect("已登记任务的句柄仍可等待");
    }

    #[tokio::test]
    async fn a_recorded_cleanup_failure_blocks_every_later_publish_and_remove() {
        let registry = ReferenceLeaseRegistry::new();
        let agent_id = Uuid::new_v4();
        let generation = Uuid::new_v4();
        let first = registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-1",
                generation,
                snapshot(),
            ))
            .await
            .expect("首次发布")
            .into_lease();

        // 任务在收尾时记载"sidecar 删除失败"：真实清理没有得到确认。
        let mut tracked = first
            .spawn_tracked({
                let lifecycle = Arc::clone(first.lifecycle());
                async move {
                    lifecycle.record_cleanup_failure(ReferenceError::new(
                        ReferenceErrorCode::EnvironmentApplyFailed,
                        "删除 git sidecar 失败：docker rm -f 返回错误",
                    ));
                }
            })
            .expect("登记清理任务");
        tracked.wait().await.expect("清理任务结束");

        // 失败粘性 + 失败代留在登记表里：连续两次 publish 都必须失败，重试不能因为登记表变空绕过。
        for attempt in 1..=2 {
            let error = registry
                .publish(AgentEnvironmentLease::new(
                    agent_id,
                    "container-2",
                    Uuid::new_v4(),
                    snapshot(),
                ))
                .await
                .expect_err("真实清理失败必须阻止发布新 generation");
            assert_eq!(
                ReferenceErrorCode::EnvironmentApplyFailed,
                error.code(),
                "第 {attempt} 次 publish"
            );
            assert_eq!(
                Some(generation),
                registry.current_generation(agent_id).await,
                "失败代不能被遗忘"
            );
            assert_eq!(
                ReferenceErrorCode::EnvironmentSuperseded,
                registry
                    .acquire(agent_id)
                    .await
                    .expect_err("撤销代不能继续对外提供")
                    .code()
            );
        }

        // remove 同样不能绕过：失败代不属于"可以干净摘除"的租约。
        for attempt in 1..=2 {
            let error = registry
                .remove(agent_id)
                .await
                .expect_err("真实清理失败也必须阻止移除");
            assert_eq!(
                ReferenceErrorCode::EnvironmentApplyFailed,
                error.code(),
                "第 {attempt} 次 remove"
            );
        }
        assert_eq!(
            Some(generation),
            registry.current_generation(agent_id).await
        );
        assert!(first.is_revoked(), "失败代的租约保持撤销");
    }

    #[tokio::test]
    async fn same_generation_publish_rejects_a_different_container() {
        let registry = ReferenceLeaseRegistry::new();
        let agent_id = Uuid::new_v4();
        let generation = Uuid::new_v4();
        registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-1",
                generation,
                snapshot(),
            ))
            .await
            .expect("首次发布");

        let error = registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-2",
                generation,
                metadata_snapshot(2),
            ))
            .await
            .expect_err("同一个 generation 不能换容器");
        assert_eq!(ReferenceErrorCode::EnvironmentApplyFailed, error.code());

        // 原租约保持可用，容器身份没有被动过。
        let current = registry.acquire(agent_id).await.expect("原租约保持可用");
        assert_eq!("container-1", current.container_id());
        assert_eq!(generation, current.generation());
    }

    #[tokio::test]
    async fn publish_rejects_reviving_a_revoked_generation() {
        let registry = ReferenceLeaseRegistry::new();
        let agent_id = Uuid::new_v4();
        let generation = Uuid::new_v4();
        let revoked = registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-1",
                generation,
                snapshot(),
            ))
            .await
            .expect("首次发布")
            .into_lease();
        // 外部（例如"停止并应用"的前半段）直接撤销这个 generation。
        revoked.revoke();

        let error = registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-1",
                generation,
                snapshot(),
            ))
            .await
            .expect_err("已撤销的 generation 不能被重新发布成可用");
        assert_eq!(ReferenceErrorCode::EnvironmentSuperseded, error.code());
        assert!(registry.acquire(agent_id).await.is_err());

        // 干净的撤销代可以被新 generation 正常接替。
        let publication = registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-2",
                Uuid::new_v4(),
                snapshot(),
            ))
            .await
            .expect("干净的撤销代可以接替");
        assert_eq!(Some(generation), publication.superseded_generation());
    }

    #[tokio::test]
    async fn concurrent_generation_swaps_wait_for_the_revoked_generation() {
        let registry = Arc::new(ReferenceLeaseRegistry::new());
        let agent_id = Uuid::new_v4();
        let first_generation = Uuid::new_v4();
        let first = registry
            .publish(AgentEnvironmentLease::new(
                agent_id,
                "container-1",
                first_generation,
                snapshot(),
            ))
            .await
            .expect("首次发布")
            .into_lease();

        // 第一个 generation 上有一个"还在跑"的资源：只有测试显式释放它才会结束。
        let (release, wait_for_release) = oneshot::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let task_finished = Arc::clone(&finished);
        let mut tracked = first
            .spawn_tracked(async move {
                let _ = wait_for_release.await;
                task_finished.store(true, Ordering::SeqCst);
            })
            .expect("登记后台资源");

        let second_generation = Uuid::new_v4();
        let third_generation = Uuid::new_v4();
        let mut second = tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .publish(AgentEnvironmentLease::new(
                        agent_id,
                        "container-2",
                        second_generation,
                        snapshot(),
                    ))
                    .await
            }
        });
        let mut third = tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .publish(AgentEnvironmentLease::new(
                        agent_id,
                        "container-3",
                        third_generation,
                        snapshot(),
                    ))
                    .await
            }
        });

        // 旧资源还阻塞时，两次并发换代都不能完成：一次在等收敛，另一次在等该 Agent 的变更锁。
        let second_early = tokio::time::timeout(Duration::from_millis(50), &mut second).await;
        let third_early = tokio::time::timeout(Duration::from_millis(50), &mut third).await;
        assert!(second_early.is_err(), "旧资源还在跑，第一次换代不能完成");
        assert!(third_early.is_err(), "并发的另一次换代必须串行在其后");
        assert!(!finished.load(Ordering::SeqCst));

        release.send(()).expect("释放后台资源");
        let second_publication = second.await.expect("换代任务结束").expect("第二次换代");
        let third_publication = third.await.expect("换代任务结束").expect("第三次换代");
        assert!(finished.load(Ordering::SeqCst));
        assert!(first.is_revoked());

        // 恰有一个赢家：当前 generation 就是它，另一个保持撤销，不会被覆盖回可用。
        let winner = registry
            .current_generation(agent_id)
            .await
            .expect("换代之后必须有当前 generation");
        assert!(winner == second_generation || winner == third_generation);
        let active = [&second_publication, &third_publication]
            .iter()
            .filter(|publication| {
                publication.lease().generation() == winner && !publication.lease().is_revoked()
            })
            .count();
        assert_eq!(1, active, "并发换代只能留下一个可用 generation");
        for publication in [&second_publication, &third_publication] {
            if publication.lease().generation() != winner {
                assert!(
                    publication.lease().is_revoked(),
                    "被取代的 generation 必须保持撤销"
                );
            }
        }
        tracked.wait().await.expect("已登记任务的句柄仍可等待");
    }
}
