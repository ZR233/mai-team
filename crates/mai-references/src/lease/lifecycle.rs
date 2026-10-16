//! generation 级异步资源生命周期：登记 fence、真实收敛等待与清理失败记载。
//!
//! 一个 generation 除了"容器 ID + 参考快照"这两条静态事实，还拥有一批**正在运行**的异步资源
//! （sidecar 容器、docker CLI 进程、后台命令）。这些资源必须在 generation 被取代时真正结束，
//! 应用才能安全地提交或发布候选环境；只丢 future 只会留下仍在写 workspace 的进程。
//!
//! 本模块把这件事收敛成三个事实：
//! - **登记**：调用方把"必须等到真实结束"的工作登记到本 generation；登记成功的任务一定在
//!   `drain` 开始计数之前被算进去。
//! - **fence**：登记与撤销共享同一把同步锁，因此撤销开始之后不可能再登记新任务——"撤销 + 等待"
//!   不会漏掉任何后来者。
//! - **失败**：只有"资源是否真的停止/删除"得不到确认时才记载清理失败；正常的有意撤销不是失败。
//!   失败是**粘性**的：记载之后每次等待都会返回同一个错误，不提供自动清除，调用方据此 fail
//!   closed，恢复路径是服务恢复或人工确认。

use std::future::Future;
use std::sync::{Mutex, MutexGuard, PoisonError};

use mai_protocol::ReferenceErrorCode;
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::error::{ReferenceError, Result};

/// 一个 generation 的异步资源生命周期。
///
/// 同一个 generation 的租约句柄共享同一个 `GenerationLifecycle`：只替换 metadata 时旧 Turn 保留
/// 自己的快照，但它登记的任务仍归属同一个 generation，因此撤销时一起被等到。
#[derive(Debug)]
pub struct GenerationLifecycle {
    generation: Uuid,
    cancellation: CancellationToken,
    /// 登记 fence：登记与撤销共享这把同步锁，两者之间没有 await 点。
    fence: Mutex<GenerationFence>,
    /// 已登记任务的真实收敛点。
    tasks: TaskTracker,
    /// 第一次真实清理失败（粘性保存）；有意取消不记在这里。
    cleanup_failure: Mutex<Option<CleanupFailure>>,
}

/// 登记 fence 的状态。
#[derive(Debug, Default)]
struct GenerationFence {
    /// 是否已经开始撤销；撤销之后拒绝一切新登记。
    revoked: bool,
}

/// 记载下来的真实清理失败。
///
/// 只保留稳定机器码与中文消息：失败是粘性的，因此每次等待都要重建同一个错误返回给调用方，
/// 而不是被取走一次就永久消失。
#[derive(Debug, Clone)]
struct CleanupFailure {
    code: ReferenceErrorCode,
    message: String,
}

/// 已经登记到某个 generation 的后台任务句柄。
///
/// 丢掉句柄不会让任务消失：任务已经计入该 generation 的真实收敛点，撤销后的等待仍会等它结束。
/// 句柄只用于调用方还活着时取回任务的返回值。
#[derive(Debug)]
pub struct TrackedTask<T> {
    handle: JoinHandle<T>,
}

impl<T> TrackedTask<T> {
    /// 等任务真实结束。
    ///
    /// 取消安全：等待被丢弃（例如调用方自己的 future 被 close/quiesce 掉）不影响任务本身，
    /// 任务仍归属本 generation，并在撤销时被等到。
    pub async fn wait(&mut self) -> std::result::Result<T, JoinError> {
        (&mut self.handle).await
    }
}

impl GenerationLifecycle {
    /// 为一个新 generation 建立空的资源生命周期。
    pub(crate) fn new(generation: Uuid) -> Self {
        Self {
            generation,
            cancellation: CancellationToken::new(),
            fence: Mutex::new(GenerationFence::default()),
            tasks: TaskTracker::new(),
            cleanup_failure: Mutex::new(None),
        }
    }

    /// 本生命周期的 generation。
    pub fn generation(&self) -> Uuid {
        self.generation
    }

    /// 本 generation 的取消令牌；撤销会触发它。
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// 是否已经开始撤销。
    pub fn is_revoked(&self) -> bool {
        lock_recovering(&self.fence).revoked
    }

    /// 登记一个必须等到真实结束的后台任务。
    ///
    /// 对外的登记入口是租约上的 `spawn_tracked`（`AgentEnvironmentLease::spawn_tracked`）：调用方
    /// 通常只持有租约，生命周期句柄用于取消、观察与记载清理失败。
    ///
    /// # Errors
    /// 已经撤销（含与撤销并发）时返回 `environment_superseded`：撤销开始后不再接受新登记，
    /// 因此调用方不会留下"撤销之后才开始、却没人等"的资源。
    pub(crate) fn spawn_tracked<F>(&self, future: F) -> Result<TrackedTask<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let fence = lock_recovering(&self.fence);
        if fence.revoked {
            return Err(ReferenceError::new(
                ReferenceErrorCode::EnvironmentSuperseded,
                format!(
                    "generation {} 已经开始撤销，拒绝登记新的后台任务",
                    self.generation
                ),
            ));
        }
        Ok(TrackedTask {
            handle: self.tasks.spawn(future),
        })
    }

    /// 同步撤销：fence 掉后续登记、取消所有等待取消的任务，并允许等待真实收敛。
    ///
    /// 它**不等**真实收敛：调用方必须继续等待收敛，否则仍可能在发布新 generation 之前留下正在
    /// 运行的旧资源。重复调用无副作用。
    pub fn revoke(&self) {
        {
            let mut fence = lock_recovering(&self.fence);
            fence.revoked = true;
        }
        self.cancellation.cancel();
        self.tasks.close();
    }

    /// 等所有已登记任务真实结束，并报告本次收敛是否干净。
    ///
    /// 只允许在 [`Self::revoke`] 之后调用：撤销之前的登记是合法的正常路径，只有撤销 fence 落下
    /// 之后，等待才代表"这个 generation 真的收敛了"。
    ///
    /// # Errors
    /// 期间记载了真实清理失败（例如 sidecar 删除失败）时返回该失败，调用方必须 fail closed：
    /// 不得据此提交或发布候选环境。失败是**粘性**的：同一个 generation 之后每一次等待都返回
    /// 同一个错误，不提供自动清除，恢复路径是服务恢复或人工确认。
    pub(crate) async fn drain(&self) -> Result<()> {
        self.tasks.wait().await;
        match lock_recovering(&self.cleanup_failure).clone() {
            Some(failure) => Err(ReferenceError::new(failure.code, failure.message)),
            None => Ok(()),
        }
    }

    /// 记载一次真实清理失败。
    ///
    /// 正常的有意撤销**不是**清理失败；只有"资源是否真的停止/删除"得不到确认时才调用本方法。
    /// 只保留第一次失败：它才是这次没能干净收敛的原始原因。记载之后不会自动清除：调用方必须
    /// 显式处理（服务恢复或人工确认），不能靠再次等待"刷新"成干净状态。
    pub fn record_cleanup_failure(&self, error: ReferenceError) {
        let mut slot = lock_recovering(&self.cleanup_failure);
        if slot.is_none() {
            *slot = Some(CleanupFailure {
                code: error.code(),
                message: error.message().to_string(),
            });
        }
    }
}

/// 取同步锁；锁被毒化说明持锁线程 panic 过，此时取回状态继续，而不是把 panic 传播给调用方。
///
/// fence 是同步的、临界区里没有 await，因此这里不会把锁带过 await 点。
fn lock_recovering<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
