//! 进程级 fatal 上报通道。
//!
//! 新 PL 架构把存储故障表达为每个 Thread 的 typed `PersistenceState`，core 会在故障世代内暂停该
//! Thread 的模型与工具准入，而不是继续假装健康。mai-server 仍保留“致命故障 -> 进程重启”的产品
//! 生命周期，因此本模块提供最小的进程级 latch：任何已知致命条件都通过 [`report_fatal`] 上报，
//! [`wait_for_fatal`] 交付给 `select_shutdown_cause`。
//!
//! 这是生命周期守卫，不是产品配置：它不读取配置、不使用 `AgentSnapshot`，也不解释任何会话数据。

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tokio::sync::Notify;

use crate::RuntimeError;

struct FatalWatch {
    message: Mutex<Option<String>>,
    changed: Notify,
}

fn watch() -> &'static Arc<FatalWatch> {
    static WATCH: OnceLock<Arc<FatalWatch>> = OnceLock::new();
    WATCH.get_or_init(|| {
        Arc::new(FatalWatch {
            message: Mutex::new(None),
            changed: Notify::new(),
        })
    })
}

/// 上报一个不可继续的致命条件，并唤醒等待者。
///
/// 只保留第一个致命条件：后续上报不覆盖最初的根因，等待者始终看到同一个终态描述。
///
/// 这是给宿主 supervisor 的接线点；在 `turn`/存储 supervisor 接入前没有 crate 内调用者。
#[allow(dead_code)]
pub(crate) fn report_fatal(error: RuntimeError) {
    let watch = watch();
    {
        let mut message = watch.message.lock().unwrap_or_else(PoisonError::into_inner);
        if message.is_none() {
            *message = Some(error.to_string());
        }
    }
    watch.changed.notify_waiters();
}

/// 等待 Runtime 进入不可继续的 fail-stop 状态。
///
/// 没有致命条件时永远等待，与旧的 repository fail-stop 等待语义一致；已上报的致命条件会立即返回
/// 同一个描述。
pub(crate) async fn wait_for_fatal() -> RuntimeError {
    let watch = watch();
    loop {
        // 先注册等待者再读取当前终态，避免 `report_fatal` 恰好发生在检查与等待之间而丢失唤醒。
        let notified = watch.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(message) = watch
            .message
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            return RuntimeError::InvalidInput(message);
        }
        notified.await;
    }
}
