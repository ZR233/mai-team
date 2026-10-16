//! 受管快照镜像的归属核对与回收。
//!
//! 换代保留可写层（`commit_stopped_environment`）与克隆出生（`commit_clone_parent`）
//! 都会产出带 `mai.team.managed=true` 与 `mai.team.snapshot.operation` 标签的镜像。
//! 这些镜像既是崩溃恢复的收据，也是可能变成孤儿的资源；本层只回答一件事：在调用方
//! 给出保护集合之后，哪些镜像的归属已经被标签、唯一名字与来源共同证明，可以回收。
//!
//! 三条硬约束：
//! - 只枚举同时带受管标签与 operation 标签的镜像，基础镜像与 legacy 临时 clone 镜像
//!   没有这两个标签，不进入回收范围；
//! - 归属不能证明时一律保留，且从不使用 `--force`、从不执行 prune；
//! - 仍被任何 daemon 容器（含未受管、已停止容器）引用，或存在子镜像依赖的镜像保留，
//!   由 daemon 在删除时做最终保护。

mod cleanup;
#[cfg(all(test, unix))]
mod fixtures;
mod inventory;
mod ownership;
mod report;
mod retention;
#[cfg(all(test, unix))]
mod tests;

pub use report::{
    RemovedSnapshotImage, RetainedSnapshotImage, SnapshotCleanupReport, SnapshotOrigin,
    SnapshotOwnershipGap, SnapshotRetentionReason,
};
pub use retention::SnapshotRetention;

/// 快照镜像的来源标签键：区分“换代保留可写层”与“克隆出生的子 Agent 父层”。
///
/// 取值见 [`SnapshotOrigin`]。缺失该标签的镜像按换代处理（本轮隔离镜像没有这个标签），
/// 但名字与另外四个标签仍必须完全一致，归属判定不放宽。
pub(crate) const SNAPSHOT_ORIGIN_LABEL_KEY: &str = "mai.team.snapshot.origin";

/// daemon 报告镜像不存在。
///
/// 核对阶段它意味着“无法证明归属”，删除阶段它意味着“目标状态已经达成”；两种情况都
/// 不是需要上抛的 Docker 故障，因此由本层统一判定。
fn is_missing_image_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("no such image") || message.contains("no such object")
}
