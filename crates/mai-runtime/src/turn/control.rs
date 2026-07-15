use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{AbortHandle, AbortRegistration, Abortable};
use mai_protocol::{SessionId, TurnId};
use tokio_util::sync::CancellationToken;

use crate::{Result, RuntimeError};

/// 单个 turn 异步任务的取消与强制终止句柄。
#[derive(Clone)]
pub(crate) struct TurnTaskHandle {
    cancellation_token: CancellationToken,
    abort_handle: Option<AbortHandle>,
}

impl TurnTaskHandle {
    pub(crate) fn spawn_with_token<F, Fut>(task: F) -> Self
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (handle, abort_registration) = Self::new_abortable();
        let task_token = handle.cancellation_token.clone();
        tokio::spawn(Abortable::new(task(task_token), abort_registration));
        handle
    }

    #[cfg(test)]
    pub(crate) fn from_external_token(cancellation_token: CancellationToken) -> Self {
        Self {
            cancellation_token,
            abort_handle: None,
        }
    }

    fn cancel(&self) {
        self.cancellation_token.cancel();
    }

    fn abort(&self) {
        if let Some(abort_handle) = &self.abort_handle {
            abort_handle.abort();
        }
    }

    fn cancel_and_abort_after(&self, grace: Duration) {
        self.cancel();
        let Some(abort_handle) = self.abort_handle.clone() else {
            return;
        };
        let cancellation_token = self.cancellation_token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            if cancellation_token.is_cancelled() {
                abort_handle.abort();
            }
        });
    }

    fn new_abortable() -> (Self, AbortRegistration) {
        let cancellation_token = CancellationToken::new();
        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        (
            Self {
                cancellation_token,
                abort_handle: Some(abort_handle),
            },
            abort_registration,
        )
    }
}

/// 当前活动 turn 的内存控制记录。
#[derive(Clone)]
pub(crate) struct TurnControl {
    pub(crate) turn_id: TurnId,
    pub(crate) session_id: SessionId,
    task_handle: TurnTaskHandle,
}

impl TurnControl {
    pub(crate) fn new(turn_id: TurnId, session_id: SessionId, task_handle: TurnTaskHandle) -> Self {
        Self {
            turn_id,
            session_id,
            task_handle,
        }
    }

    pub(crate) fn cancel_task(&self) {
        self.task_handle.cancel();
    }

    pub(crate) fn abort_task(&self) {
        self.task_handle.abort();
    }

    pub(crate) fn cancel_task_and_abort_after(&self, grace: Duration) {
        self.task_handle.cancel_and_abort_after(grace);
    }
}

/// 并发保存当前活动 turn 的 slot。
#[derive(Clone, Default)]
pub(crate) struct TurnControlSlot {
    inner: Arc<Mutex<Option<TurnControl>>>,
}

impl TurnControlSlot {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn with_active(control: TurnControl) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(control))),
        }
    }

    pub(crate) fn set(&self, control: TurnControl) -> Option<TurnControl> {
        self.inner
            .lock()
            .expect("active turn slot lock")
            .replace(control)
    }

    pub(crate) fn current(&self) -> Option<TurnControl> {
        self.inner.lock().expect("active turn slot lock").clone()
    }

    pub(crate) fn take_if_turn(&self, turn_id: &TurnId) -> Option<TurnControl> {
        let mut current = self.inner.lock().expect("active turn slot lock");
        if current
            .as_ref()
            .is_some_and(|control| control.turn_id == *turn_id)
        {
            current.take()
        } else {
            None
        }
    }
}

/// 长流程副作用前后用于校验 turn 当前性的保护信息。
#[derive(Clone)]
pub(crate) struct TurnGuard {
    turn_id: TurnId,
    cancellation_token: CancellationToken,
}

impl TurnGuard {
    pub(crate) fn new(turn_id: TurnId, cancellation_token: CancellationToken) -> Self {
        Self {
            turn_id,
            cancellation_token,
        }
    }

    pub(crate) fn ensure_current(&self, current_turn: Option<&TurnId>) -> Result<()> {
        ensure_not_cancelled(&self.cancellation_token)?;
        if current_turn == Some(&self.turn_id) {
            Ok(())
        } else {
            Err(RuntimeError::TurnCancelled)
        }
    }
}

pub(crate) fn ensure_not_cancelled(cancellation_token: &CancellationToken) -> Result<()> {
    if cancellation_token.is_cancelled() {
        Err(RuntimeError::TurnCancelled)
    } else {
        Ok(())
    }
}
