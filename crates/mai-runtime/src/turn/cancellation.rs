use tokio_util::sync::CancellationToken;

use crate::{Result, RuntimeError};

/// 在产品长流程副作用边界检查 PL runtime 注入的取消信号。
pub(crate) fn ensure_not_cancelled(token: &CancellationToken) -> Result<()> {
    if token.is_cancelled() {
        Err(RuntimeError::TurnCancelled)
    } else {
        Ok(())
    }
}
