use std::future::Future;
use std::sync::Arc;

use mai_protocol::AgentId;
use mai_references::AgentAdmission;

use super::{AgentPurgeOps, descendant_delete_order, purge_agent_records};
use crate::{Result, RuntimeError};

/// 父退役 fence 的采样上限。
///
/// 每轮采样都必须退役至少一个新节点，否则立刻结束；达到上限说明有并发的 spawn 在删除期间
/// 持续制造后代，此时显式失败让调用方重试，而不是宣称删除完成。
const DELETE_FENCE_SAMPLE_LIMIT: usize = 8;

/// 统一 agent 删除时 canonical runtime 与产品资源之间的生命周期边界。
///
/// 删除是明确授权的资源删除，因此每个节点都必须走完整的状态跃迁：先关闭准入，再取得
/// 它自己的生命周期锁（等待在飞的 apply 结束），然后关闭 canonical Thread、撤销租约、
/// 按精确容器 ID 删除容器与 MCP sidecar、回收该 Agent 自己的工作区，最后才删除产品记录。
/// 任何一步失败都必须向上传播，保留可重试状态；实现不得吞掉容器或卷的失败。
pub(crate) trait AgentDeleteOps: AgentPurgeOps {
    /// 该 Agent 的准入记录：删除在它的 `lifecycle` 锁内串行化，并由它的 gate 阻止新 Turn。
    fn agent_admission(&self, agent_id: AgentId) -> Arc<AgentAdmission>;

    /// 关闭 canonical Thread；失败时必须保留原 owner，让调用方能用同一 id 重试。
    fn close_canonical_agent(&self, agent_id: AgentId) -> impl Future<Output = Result<()>> + Send;

    /// 撤销并移除该 Agent 的固定环境租约，使残存句柄立即失效。
    fn revoke_agent_environment_lease(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<()>> + Send;

    /// 按精确容器 ID 删除该 Agent 的容器与 MCP sidecar，并回收它自己的 workspace 卷。
    ///
    /// 调用方保证不会跨节点持有生命周期锁；实现必须在本节点范围内完成清理，不得顺手删除
    /// 其它 Agent 的容器、卷或记录。
    fn delete_agent_runtime_resources(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// 删除 `root_id` 及其整棵子树的产品记录与运行资源。
///
/// 先做非阻塞退役 fence：把采样到的所有节点标记为"正在删除"，再反复采样直到确认没有新的
/// 后代，然后才逐个节点关闭与清理。这样任何在其父节点被清理之后才提交注册的 child 都会在
/// 提交前观测到父节点的退役标记并回滚自己，不会留下没人再能找到的孤儿容器/卷。
/// 标记只增加内存计数、绝不等待任何锁，因此不会与"apply 等当前 Turn"形成互等。
///
/// 处理顺序严格 child-first：每个节点都在自己的生命周期锁内完成关闭与清理，处理完一个节点
/// 就释放它的锁，因此不会跨父子同时持有两把锁，也不会阻塞协作 spawn 或应用状态机。
pub(crate) async fn delete_agent(ops: &impl AgentDeleteOps, root_id: AgentId) -> Result<()> {
    let mut retired = Vec::new();
    let mut retirements = Vec::new();
    let mut attempts = 0;
    let targets = loop {
        let targets = descendant_delete_order(root_id, &ops.agent_summaries().await)?;
        if targets.iter().all(|agent_id| retired.contains(agent_id)) {
            break targets;
        }
        attempts += 1;
        if attempts > DELETE_FENCE_SAMPLE_LIMIT {
            return Err(RuntimeError::InvalidInput(format!(
                "agent `{root_id}` keeps spawning descendants while it is being deleted; retry the deletion"
            )));
        }
        for agent_id in &targets {
            if !retired.contains(agent_id) {
                retirements.push(ops.agent_admission(*agent_id).retire());
                retired.push(*agent_id);
            }
        }
    };
    for agent_id in targets {
        delete_agent_node(ops, agent_id).await?;
    }
    drop(retirements);
    Ok(())
}

/// 创建回滚：尚未注册的 Agent 没有 canonical Thread owner，只释放运行资源与产品记录。
pub(crate) async fn rollback_unregistered_agent(
    ops: &impl AgentDeleteOps,
    agent_id: AgentId,
) -> Result<()> {
    let admission = ops.agent_admission(agent_id);
    let _admission_hold = admission.gate.close();
    let _lifecycle = admission.lifecycle.lock().await;
    release_agent_resources_and_records(ops, agent_id).await
}

/// 删除单个节点的完整跃迁；两个 guard 都在本函数返回时释放。
///
/// 先关 gate 再取生命周期锁：新的 Turn 立刻被挡在门外，而在飞的 apply 会在锁上排队，
/// 删除等它结束之后才开始动资源。
async fn delete_agent_node(ops: &impl AgentDeleteOps, agent_id: AgentId) -> Result<()> {
    let admission = ops.agent_admission(agent_id);
    let _admission_hold = admission.gate.close();
    let _lifecycle = admission.lifecycle.lock().await;
    ops.close_canonical_agent(agent_id).await?;
    release_agent_resources_and_records(ops, agent_id).await
}

/// 撤销租约、删除运行资源与工作区，最后删除产品记录；调用方必须已持有该节点的生命周期锁。
async fn release_agent_resources_and_records(
    ops: &impl AgentDeleteOps,
    agent_id: AgentId,
) -> Result<()> {
    ops.revoke_agent_environment_lease(agent_id).await?;
    ops.delete_agent_runtime_resources(agent_id).await?;
    purge_agent_records(ops, agent_id).await
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::Arc;

    use chrono::{DateTime, Utc};
    use mai_protocol::{AgentResourceSnapshot, AgentSummary, RuntimeUsageSnapshot};
    use pretty_assertions::assert_eq;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use super::*;
    use crate::RuntimeError;

    struct FakeDeleteOps {
        /// 按顺序返回的树快照；用完后一直返回最后一份（稳定态）。
        snapshots: Mutex<VecDeque<Vec<AgentSummary>>>,
        admissions: HashMap<AgentId, Arc<AgentAdmission>>,
        calls: Arc<Mutex<Vec<String>>>,
        fail_close: Option<AgentId>,
    }

    impl FakeDeleteOps {
        fn new(summaries: Vec<AgentSummary>) -> Self {
            Self::with_snapshots(vec![summaries])
        }

        fn with_snapshots(snapshots: Vec<Vec<AgentSummary>>) -> Self {
            let admissions = snapshots
                .iter()
                .flatten()
                .map(|summary| (summary.id, Arc::new(AgentAdmission::default())))
                .collect();
            Self {
                snapshots: Mutex::new(VecDeque::from(snapshots)),
                admissions,
                calls: Arc::new(Mutex::new(Vec::new())),
                fail_close: None,
            }
        }

        async fn record(&self, entry: impl Into<String>) {
            self.calls.lock().await.push(entry.into());
        }

        /// 记录调用时同时持有生命周期锁的节点数量；正确实现每次都必须恰好是 1。
        fn held_lifecycles(&self) -> usize {
            self.admissions
                .values()
                .filter(|admission| admission.lifecycle.try_lock().is_err())
                .count()
        }

        /// 记录调用时已经被退役的节点数量；正确实现只在删除流程内为整棵子树的节点置位。
        fn retired_nodes(&self) -> usize {
            self.admissions
                .values()
                .filter(|admission| admission.is_retired())
                .count()
        }
    }

    impl AgentPurgeOps for FakeDeleteOps {
        async fn agent_summaries(&self) -> Vec<AgentSummary> {
            let mut snapshots = self.snapshots.lock().await;
            if snapshots.len() > 1 {
                return snapshots.pop_front().expect("checked snapshot length");
            }
            snapshots.front().cloned().unwrap_or_default()
        }

        async fn delete_agent_from_store(&self, agent_id: AgentId) -> Result<()> {
            self.record(format!("store:{agent_id}")).await;
            Ok(())
        }

        async fn delete_agent_artifacts(&self, agent_id: AgentId) -> Result<()> {
            self.record(format!("artifacts:{agent_id}")).await;
            Ok(())
        }

        async fn remove_agent_from_memory(&self, agent_id: AgentId) {
            self.record(format!("memory:{agent_id}")).await;
        }

        async fn publish_agent_deleted(&self, agent_id: AgentId) {
            self.record(format!("event:{agent_id}")).await;
        }
    }

    impl AgentDeleteOps for FakeDeleteOps {
        fn agent_admission(&self, agent_id: AgentId) -> Arc<AgentAdmission> {
            Arc::clone(
                self.admissions
                    .get(&agent_id)
                    .expect("fake admission for every tree node"),
            )
        }

        async fn close_canonical_agent(&self, agent_id: AgentId) -> Result<()> {
            self.record(format!(
                "close:{agent_id}:locks={}:retired={}",
                self.held_lifecycles(),
                self.retired_nodes()
            ))
            .await;
            if self.fail_close == Some(agent_id) {
                return Err(RuntimeError::InvalidInput(
                    "canonical close failed".to_string(),
                ));
            }
            Ok(())
        }

        async fn revoke_agent_environment_lease(&self, agent_id: AgentId) -> Result<()> {
            self.record(format!("lease:{agent_id}:locks={}", self.held_lifecycles()))
                .await;
            Ok(())
        }

        async fn delete_agent_runtime_resources(&self, agent_id: AgentId) -> Result<()> {
            self.record(format!(
                "resources:{agent_id}:locks={}",
                self.held_lifecycles()
            ))
            .await;
            Ok(())
        }
    }

    /// 缺陷敏感：旧实现只锁 root，且 purge 完全绕过后代锁；这里要求每个节点在动资源之前
    /// 就已经被退役（spawn 侧据此拒绝新 child），关闭、撤租与资源删除都发生在它自己的
    /// 生命周期锁内，且同一时刻只持有一个节点的锁。
    #[tokio::test]
    async fn delete_releases_every_node_child_first_under_its_own_lifecycle_lock() {
        let base = Utc::now();
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        let grandchild = Uuid::new_v4();
        let unrelated = Uuid::new_v4();
        let ops = FakeDeleteOps::new(vec![
            summary(root, None, base),
            summary(child, Some(root), base + chrono::Duration::seconds(1)),
            summary(grandchild, Some(child), base + chrono::Duration::seconds(2)),
            summary(unrelated, None, base + chrono::Duration::seconds(3)),
        ]);

        delete_agent(&ops, root).await.expect("delete agent tree");

        let mut expected = Vec::new();
        for agent_id in [grandchild, child, root] {
            expected.push(format!("close:{agent_id}:locks=1:retired=3"));
            expected.push(format!("lease:{agent_id}:locks=1"));
            expected.push(format!("resources:{agent_id}:locks=1"));
            expected.push(format!("artifacts:{agent_id}"));
            expected.push(format!("store:{agent_id}"));
            expected.push(format!("memory:{agent_id}"));
            expected.push(format!("event:{agent_id}"));
        }
        let calls = ops.calls.lock().await.clone();
        assert_eq!(expected, calls);
        // 无关的另一棵树既不在删除目标里，也不会被顺手退役。
        assert!(
            !calls
                .iter()
                .any(|call| call.contains(&unrelated.to_string())),
            "unrelated tree must stay untouched: {calls:?}"
        );
    }

    /// 缺陷敏感：退役标记只覆盖首次采样，随后注册进来的 child 会被漏删。
    /// 正确实现必须在标记之后继续采样、把窗口内新增的后代补进删除范围。
    #[tokio::test]
    async fn delete_retires_and_covers_descendants_spawned_between_samples() {
        let base = Utc::now();
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        let late_child = Uuid::new_v4();
        let ops = FakeDeleteOps::with_snapshots(vec![
            vec![
                summary(root, None, base),
                summary(child, Some(root), base + chrono::Duration::seconds(1)),
            ],
            vec![
                summary(root, None, base),
                summary(child, Some(root), base + chrono::Duration::seconds(1)),
                summary(late_child, Some(child), base + chrono::Duration::seconds(2)),
            ],
            vec![
                summary(root, None, base),
                summary(child, Some(root), base + chrono::Duration::seconds(1)),
                summary(late_child, Some(child), base + chrono::Duration::seconds(2)),
            ],
        ]);

        delete_agent(&ops, root).await.expect("delete agent tree");

        let calls = ops.calls.lock().await.clone();
        assert!(
            calls.contains(&format!("close:{late_child}:locks=1:retired=3")),
            "late descendant must be retired and closed: {calls:?}"
        );
        assert_eq!(
            vec![
                format!("close:{late_child}:locks=1:retired=3"),
                format!("lease:{late_child}:locks=1"),
                format!("resources:{late_child}:locks=1"),
                format!("artifacts:{late_child}"),
                format!("store:{late_child}"),
                format!("memory:{late_child}"),
                format!("event:{late_child}"),
                format!("close:{child}:locks=1:retired=3"),
                format!("lease:{child}:locks=1"),
                format!("resources:{child}:locks=1"),
                format!("artifacts:{child}"),
                format!("store:{child}"),
                format!("memory:{child}"),
                format!("event:{child}"),
                format!("close:{root}:locks=1:retired=3"),
                format!("lease:{root}:locks=1"),
                format!("resources:{root}:locks=1"),
                format!("artifacts:{root}"),
                format!("store:{root}"),
                format!("memory:{root}"),
                format!("event:{root}"),
            ],
            calls
        );
    }

    /// canonical Thread 关闭失败必须留在可重试状态：不得删除资源，也不得删掉任何记录。
    #[tokio::test]
    async fn canonical_close_failure_keeps_every_record_and_resource() {
        let base = Utc::now();
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        let mut ops = FakeDeleteOps::new(vec![
            summary(root, None, base),
            summary(child, Some(root), base + chrono::Duration::seconds(1)),
        ]);
        ops.fail_close = Some(child);

        delete_agent(&ops, root)
            .await
            .expect_err("canonical close must fail the deletion");

        let calls = ops.calls.lock().await.clone();
        assert!(
            calls.contains(&format!("close:{child}:locks=1:retired=2")),
            "child close must run under the child lifecycle lock: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|call| call.starts_with(&format!("resources:{child}"))),
            "child resources must survive a failed canonical close: {calls:?}"
        );
        assert!(
            !calls.iter().any(|call| call.ends_with(&root.to_string())),
            "root must not be touched after a descendant failure: {calls:?}"
        );
    }

    /// 有并发 spawn 持续制造后代时不能宣称删除完成：达到采样上限必须显式失败，
    /// 而且在采样收敛之前不得动任何资源。
    #[tokio::test]
    async fn delete_fails_closed_when_the_tree_keeps_growing() {
        let base = Utc::now();
        let root = Uuid::new_v4();
        let mut snapshots = Vec::new();
        let mut tree = vec![summary(root, None, base)];
        for index in 0..(DELETE_FENCE_SAMPLE_LIMIT + 2) {
            tree.push(summary(
                Uuid::new_v4(),
                Some(root),
                base + chrono::Duration::seconds(index as i64 + 1),
            ));
            snapshots.push(tree.clone());
        }
        let ops = FakeDeleteOps::with_snapshots(snapshots);

        delete_agent(&ops, root)
            .await
            .expect_err("a growing tree must fail closed");

        assert!(
            ops.calls.lock().await.is_empty(),
            "采样没有收敛之前不能动任何资源"
        );
    }

    /// 创建回滚没有 canonical Thread owner：仍必须在生命周期锁内释放资源并清除记录。
    #[tokio::test]
    async fn rollback_unregistered_agent_releases_resources_before_records() {
        let agent_id = Uuid::new_v4();
        let ops = FakeDeleteOps::new(vec![summary(agent_id, None, Utc::now())]);

        rollback_unregistered_agent(&ops, agent_id)
            .await
            .expect("rollback unregistered agent");

        assert_eq!(
            vec![
                format!("lease:{agent_id}:locks=1"),
                format!("resources:{agent_id}:locks=1"),
                format!("artifacts:{agent_id}"),
                format!("store:{agent_id}"),
                format!("memory:{agent_id}"),
                format!("event:{agent_id}"),
            ],
            *ops.calls.lock().await
        );
    }

    fn summary(id: AgentId, parent_id: Option<AgentId>, created_at: DateTime<Utc>) -> AgentSummary {
        AgentSummary {
            id,
            parent_id,
            task_id: None,
            project_id: None,
            role: None,
            profile_id: None,
            workspace: None,
            review_run_id: None,
            name: "agent".to_string(),
            resource: AgentResourceSnapshot::default(),
            runtime: None,
            last_turn: None,
            container_id: None,
            docker_image: "unused".to_string(),
            provider_id: "mock".to_string(),
            provider_name: "Mock".to_string(),
            model: "mock-model".to_string(),
            reasoning_effort: None,
            created_at,
            updated_at: created_at,
            usage: RuntimeUsageSnapshot::default(),
        }
    }
}
