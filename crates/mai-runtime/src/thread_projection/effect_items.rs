//! Effect/context typed 事实到产品条目状态与 identity 的纯映射。
//!
//! 这些函数只做类型转换，不读存储、不持有状态；归并逻辑见 [`super::history`]。

use pl_core::context::{ContextContent, OpaquePayload};
use pl_core::thread::{
    ToolDelivery, ToolOutcome,
    task::{TaskRecord, TaskStatus},
};
use pl_protocol::{
    CancelledThreadTool, FailedThreadTool, InterruptedThreadTool, RunningThreadTool,
    SucceededThreadTool, ThreadContentLifecycle, ThreadItemState, ThreadRawPayload,
    ThreadTextChannel, ThreadTextItem, ThreadToolFailure, ThreadToolFailureKind, ThreadToolOutput,
    ThreadToolState,
};

/// 从 canonical content 拼接可见正文；非文本内容不参与拼接。
pub(super) fn context_text(content: &[ContextContent]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContextContent::Text { text } => Some(text.as_ref()),
            ContextContent::Resource { .. } | ContextContent::Opaque { .. } => None,
        })
        .collect()
}

/// 生成文本条目 state；正文为空时返回 `None`，不产生空正文条目。
pub(super) fn text_state(
    channel: ThreadTextChannel,
    content: &[ContextContent],
    lifecycle: ThreadContentLifecycle,
) -> Option<ThreadItemState> {
    let text = context_text(content);
    (!text.is_empty())
        .then(|| ThreadItemState::Text(ThreadTextItem::new(channel, text, Vec::new(), lifecycle)))
}

pub(super) fn running_tool() -> ThreadToolState {
    ThreadToolState::Running(RunningThreadTool::new(String::new()))
}

pub(super) fn succeeded(at: i64, result: String) -> ThreadToolState {
    ThreadToolState::Succeeded(SucceededThreadTool::new(
        at,
        ThreadToolOutput::new(result, Vec::new(), Vec::new(), None),
    ))
}

pub(super) fn failed_tool(at: i64, message: String) -> ThreadToolState {
    ThreadToolState::Failed(FailedThreadTool::new(
        at,
        ThreadToolFailure::new(ThreadToolFailureKind::Execution, message),
        None,
    ))
}

/// task 生命周期 → 产品工具状态；工具结果到达前只表示“正在执行”。
pub(super) fn task_state(task: &TaskRecord, at: i64) -> ThreadToolState {
    match task.status {
        TaskStatus::Running | TaskStatus::Succeeded => running_tool(),
        TaskStatus::Failed => failed_tool(at, "tool task failed".to_owned()),
        TaskStatus::Cancelled => ThreadToolState::Cancelled(CancelledThreadTool::new(
            at,
            "tool task cancelled".to_owned(),
        )),
        TaskStatus::Interrupted => ThreadToolState::Interrupted(InterruptedThreadTool::new(
            at,
            "tool task interrupted".to_owned(),
        )),
    }
}

/// delivery 是 core 对工具终态的权威描述。
pub(super) fn delivery_state(delivery: &ToolDelivery, at: i64) -> ThreadToolState {
    match &delivery.outcome {
        ToolOutcome::Succeeded => succeeded(at, delivery_text(delivery)),
        ToolOutcome::Cancelled => ThreadToolState::Cancelled(CancelledThreadTool::new(
            at,
            "tool call cancelled".to_owned(),
        )),
        ToolOutcome::Interrupted => ThreadToolState::Interrupted(InterruptedThreadTool::new(
            at,
            "tool call interrupted".to_owned(),
        )),
        ToolOutcome::Failed(error) => failed_tool(at, error.to_string()),
    }
}

fn delivery_text(delivery: &ToolDelivery) -> String {
    let delivered = context_text(&delivery.delivered_context);
    if delivered.is_empty() {
        delivery.output.payload().content().to_owned()
    } else {
        delivered
    }
}

pub(super) fn raw_payload(payload: &OpaquePayload) -> ThreadRawPayload {
    ThreadRawPayload {
        format: payload.format().to_owned(),
        version: payload.version(),
        content: payload.content().to_owned(),
    }
}

pub(super) fn turn_item_id(turn_id: &str) -> String {
    format!("turn:{}:{turn_id}", turn_id.len())
}

pub(super) fn tool_item_id(call_id: &str) -> String {
    format!("tool:{}:{call_id}", call_id.len())
}

pub(super) fn skill_item_id(call_id: &str) -> String {
    format!("skill:{}:{call_id}", call_id.len())
}

pub(super) fn completion_item_id(call_id: &str) -> String {
    format!("completion:{}:{call_id}", call_id.len())
}

pub(super) fn response_text_id(attempt_id: &str) -> String {
    format!("model:{}:{attempt_id}:text", attempt_id.len())
}
