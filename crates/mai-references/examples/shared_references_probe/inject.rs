//! 崩溃注入：在用户指定的 phase 已经落盘收据之后终止进程一次。
//!
//! 契约（与 `ReferenceApplicationObserver::recorded` 的语义一致）：
//! - 只观察已经提交的收据，不改配置、不释放准入、不代替资源所有者；
//! - 先写“已触发”标记并 fsync（文件与目录），再硬退出 `137`；
//! - 同一 phase 只注入一次：标记存在时后续运行不再注入，避免恢复过程被无限 kill。

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use mai_protocol::ReferenceApplyPhase;
use mai_references::ReferenceApplicationObserver;
use mai_store::PersistedReferenceOperation;
use serde_json::json;
use uuid::Uuid;

use crate::error::{ProbeError, Result};

/// 在指定 phase 触发一次的观察者。
pub struct PhaseCrashInjector {
    phase: ReferenceApplyPhase,
    /// 只对这一次操作生效；`None` 表示对本进程驱动的任意操作生效。
    operation: Option<Uuid>,
    marker: PathBuf,
    armed: AtomicBool,
}

impl PhaseCrashInjector {
    /// 创建观察者；`marker` 是“已注入”标记文件。
    pub fn new(phase: ReferenceApplyPhase, operation: Option<Uuid>, marker: PathBuf) -> Self {
        Self {
            phase,
            operation,
            marker,
            armed: AtomicBool::new(true),
        }
    }

    /// 标记文件路径。
    pub fn marker(&self) -> &std::path::Path {
        &self.marker
    }
}

impl ReferenceApplicationObserver for PhaseCrashInjector {
    fn recorded(&self, receipt: &PersistedReferenceOperation) {
        if !self.armed.load(Ordering::SeqCst) {
            return;
        }
        if receipt.operation.phase != self.phase {
            return;
        }
        if let Some(operation) = self.operation
            && receipt.operation.id != operation
        {
            return;
        }
        // 已经注入过：不再 kill，把恢复留给 root 反复运行 recover。
        if self.marker.exists() {
            return;
        }
        if !self.armed.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Err(error) = write_marker(&self.marker, receipt) {
            eprintln!("注入标记写入失败（继续按验收要求退出）：{error}");
        }
        let _ = std::io::stdout().flush();
        eprintln!(
            "[inject] phase {} 的收据已落盘（operation {}），按验收要求在此时终止进程",
            receipt.operation.phase, receipt.operation.id
        );
        exit_hard(137);
    }
}

/// 写“已触发”标记并 fsync 文件与父目录；下一步才允许退出。
fn write_marker(path: &std::path::Path, receipt: &PersistedReferenceOperation) -> Result<()> {
    if let Some(parent) = path.parent() {
        crate::guard::ensure_directory(parent)?;
    }
    let payload = json!({
        "probe": "mai-references-acceptance",
        "phase": receipt.operation.phase.to_string(),
        "operation_id": receipt.operation.id,
        "agent_id": receipt.operation.agent_id,
        "project_id": receipt.operation.project_id,
        "target_revision": receipt.operation.target_revision,
        "old_container_id": receipt.operation.old_container_id,
        "candidate_container_id": receipt.operation.candidate_container_id,
        "snapshot_image_id": receipt.operation.snapshot_image_id,
        "injected_at": Utc::now(),
    });
    let text = serde_json::to_string_pretty(&payload)
        .map_err(|error| ProbeError::new("injection_marker_serialize_failed", error.to_string()))?;
    let mut file = std::fs::File::create(path).map_err(|error| {
        ProbeError::new(
            "injection_marker_create_failed",
            format!("无法创建注入标记 {}：{error}", path.display()),
        )
    })?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        let directory = std::fs::File::open(parent).map_err(|error| {
            ProbeError::new(
                "injection_marker_sync_failed",
                format!("无法打开标记目录 {}：{error}", parent.display()),
            )
        })?;
        directory.sync_all()?;
    }
    Ok(())
}

/// 硬退出，不运行任何析构（模拟进程被立刻杀掉）。
#[cfg(unix)]
fn exit_hard(code: i32) -> ! {
    // SAFETY: `_exit` 只结束当前进程，不触碰共享的可变内存。
    unsafe { libc::_exit(code) }
}

#[cfg(not(unix))]
fn exit_hard(code: i32) -> ! {
    std::process::exit(code)
}
