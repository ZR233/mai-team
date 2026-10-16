//! 每个 Agent 的准入原因和生命周期锁；与产品输入队列的暂停状态独立。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mai_protocol::AgentId;
use pl_core::thread::admission::{TurnAdmissionGate, TurnAdmissionHold};

#[derive(Debug, Default)]
pub struct ReferenceAdmissions(Mutex<HashMap<AgentId, Arc<AgentAdmission>>>);

#[derive(Debug, Default)]
pub struct AgentAdmission {
    pub lifecycle: tokio::sync::Mutex<()>,
    pub gate: TurnAdmissionGate,
    pending: Mutex<Option<(u64, TurnAdmissionHold)>>,
    retired: AtomicUsize,
}

/// 删除流程持有的退役标记 guard：最后一个 guard 被丢弃时撤销标记。
///
/// 这是纯内存、非阻塞的 fence：`retire()` 只增加计数，任何路径都可以立刻用 `is_retired()`
/// 判断"是否有删除正在处理该 Agent"，既不会去抢 lifecycle 锁、也不会等待 gate，因此不会与
/// "apply 等当前 Turn、当前 Turn 又在 spawn"形成互等。计数式保证并发删除同一棵子树时标记
/// 不会被先结束的那一个提前撤销。
#[derive(Debug)]
pub struct AgentRetirement {
    admission: Arc<AgentAdmission>,
}

impl Drop for AgentRetirement {
    fn drop(&mut self) {
        self.admission.retired.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ReferenceAdmissions {
    pub fn agent(&self, id: AgentId) -> Arc<AgentAdmission> {
        Arc::clone(
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(id)
                .or_default(),
        )
    }
}

impl AgentAdmission {
    /// 标记该 Agent 正在被删除，并返回在本次删除结束时撤销标记的 guard。
    ///
    /// 只做一次原子加一：调用方顺序不受锁影响，可以在动任何资源之前先把整棵子树的节点
    /// 标记完，从而让随后提交注册的 child 一定能观测到父节点的退役。
    pub fn retire(self: &Arc<Self>) -> AgentRetirement {
        self.retired.fetch_add(1, Ordering::AcqRel);
        AgentRetirement {
            admission: Arc::clone(self),
        }
    }

    /// 是否有删除正在处理该 Agent。只读取内存计数，不加锁、不等待。
    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Acquire) > 0
    }

    /// 在提交配置之前调用；即使当前 Turn 仍在运行，下一 Turn 也不能越过 gate。
    pub fn block(&self, revision: u64) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match pending.as_mut() {
            Some((target, _hold)) => *target = (*target).max(revision),
            None => *pending = Some((revision, self.gate.close())),
        }
    }

    /// 只有对应目标或更新目标已发布，才能释放本模块拥有的原因。
    pub fn published(&self, revision: u64) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending
            .as_ref()
            .is_some_and(|(target, _)| *target <= revision)
        {
            pending.take();
        }
    }

    pub fn pending_revision(&self) -> Option<u64> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|(revision, _)| *revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn stale_publication_cannot_release_newer_target_or_queue_hold() {
        let admission = AgentAdmission::default();
        let independent = admission.gate.close();
        admission.block(4);
        admission.block(5);
        admission.published(4);
        assert_eq!(admission.pending_revision(), Some(5));
        admission.published(5);
        assert_eq!(admission.pending_revision(), None);
        assert!(!admission.gate.is_open());
        drop(independent);
        assert!(admission.gate.is_open());
    }

    /// 缺陷敏感：退休标记必须非阻塞（不占 lifecycle 锁）且并发删除共享计数，
    /// 否则"标记还没撤销就被别的路径读成未退役"会重新打开 spawn/delete 的竞态窗口。
    #[test]
    fn retirement_marker_is_non_blocking_and_reference_counted() {
        let admission = Arc::new(AgentAdmission::default());
        assert!(!admission.is_retired());

        let first = admission.retire();
        assert!(admission.is_retired());
        assert!(
            admission.lifecycle.try_lock().is_ok(),
            "退休标记不能占用 lifecycle 锁"
        );
        assert!(admission.gate.is_open(), "退休标记不能改动准入 gate");

        let second = admission.retire();
        drop(first);
        assert!(admission.is_retired(), "还有删除在进行时不能提前撤销标记");
        drop(second);
        assert!(!admission.is_retired());
    }
}
