//! mai 产品的纯只读 Thread 状态与 Turn 投影。
//!
//! 本模块把 PL core 的 canonical 事实投影成 `mai_protocol` 重导出的 `pl_protocol` 产品 DTO。
//! 输入都是调用方通过 pl-core typed API 取得的事实：当前状态是
//! [`pl_core::thread::ThreadSnapshot`]，已提交的 history 批次是
//! [`pl_core::thread::ThreadEffectBatch`]；输出是 [`mai_protocol::ThreadSnapshot`]、
//! [`mai_protocol::Turn`]、[`mai_protocol::ThreadTurnPage`] 等产品 DTO。
//!
//! 设计边界：
//! - 只读：不驱动模型、工具或持久化，不读取任何会话 SQLite，也不解码存储键；
//! - 事实来源唯一：身份、pending input、interaction、storage fault 与 usage 语义全部来自 PL
//!   core 的 typed 事实，绝不从旧的 `AgentSnapshot` / `ThreadCommit` 反推；
//! - 未知状态显式报错（[`ProjectionError`]），不静默回退成占位值；
//! - 项目、角色、标题、Mode 与工作区等元数据来自 mai 的 [`mai_protocol::AgentSummary`] 与调用
//!   方传入的 [`ThreadProjectionMetadata`]，PL 不管理这些产品配置。
//!
//! 消费入口见 [`project_snapshot`]（权威首帧）、[`project_turn`]（Turn 帧）、
//! [`project_turn_history_from_effects`]（按 effect 归并的 Turn history 页）与 [`status`]。

mod effect_items;
mod history;
mod snapshot;
mod turns;

pub(crate) use history::{project_turn_history_from_effects, turn_terminal_sequence};
pub(crate) use snapshot::{ThreadProjectionMetadata, project_snapshot, status};
pub(crate) use turns::project_turn;

/// 投影无法由 core typed 事实确定地完成时的显式错误。
///
/// 每一个变体都对应“core 里存在一个投影不认识的 typed 事实”，而不是解析文本失败：mai 宁可让
/// 调用方看到缺口，也不伪造一个看起来健康的产品快照。
#[derive(Debug, thiserror::Error)]
pub(crate) enum ProjectionError {
    /// user-input interaction 的 payload 不是 PL 内建 `pl.tool.user-input` v1 编码。
    #[error("unsupported Thread interaction payload `{format}` version {version}")]
    UnsupportedInteraction { format: String, version: u32 },
    /// interaction payload 是该格式，但内容无法解码为产品 question 列表。
    #[error("Thread interaction payload cannot be decoded")]
    Interaction(#[from] serde_json::Error),
    /// Thread 仍持有一个 mai 无法投影的未决 Permission（mai 没有工具审批 payload 契约）。
    #[error(
        "Thread {thread_id} still holds an unresolved permission `{permission_id}` that mai cannot project"
    )]
    UnsupportedPermission {
        thread_id: String,
        permission_id: String,
    },
    /// 常驻的 interaction 记录不是 `Pending`，说明 core 与投影对“还在等待回答”的假设不一致。
    #[error("resident Thread interaction `{0}` is not pending")]
    InteractionNotPending(String),
    /// interaction 记录的请求身份与它的 map key 不一致。
    #[error("Thread interaction `{0}` request identity does not match its key")]
    InteractionIdentity(String),
    /// 计数超出产品 DTO 的表示范围。
    #[error("count exceeds the product representation")]
    Count,
    /// 到达预算的 Turn 没有可用的最终时长。
    #[error("Turn {0} has no measured final duration")]
    MissingDuration(String),
}
