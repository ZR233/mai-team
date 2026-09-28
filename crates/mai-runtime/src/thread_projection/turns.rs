//! Turn 生命周期、阶段与诊断的只读投影。
//!
//! 输入是 PL core 已提交的 [`TurnRecord`] 及所在 Thread 的当前 typed 快照，输出是产品
//! [`mai_protocol::Turn`]。终态、运行阶段与失败都由 core 事实派生，不从旧 `AgentSnapshot` /
//! `ThreadCommit` 反推，也不读取任何存储。

use mai_protocol::{Turn, TurnPhase, TurnState};
use pl_core::thread::{
    AttemptOutcome, ThreadSnapshot as CoreThreadSnapshot, TurnOutcome as CoreTurnOutcome,
    TurnRecord, TurnState as CoreTurnState, task::TaskStatus,
};
use pl_protocol::{
    BudgetLimitKind, BudgetLimitSnapshot, BudgetLimitedTurnState, BudgetUsage, CancelledTurnState,
    CompletedTurnState, FailedTurnState, ProviderFailureKind, RunningTurnState,
    TurnCancellationCause, TurnCompletion, TurnFailure, TurnFailureCategory, TurnRolloverOutcome,
};

use super::ProjectionError;

/// 一次投影所用的时间戳。
struct Stamp {
    started_at: i64,
    updated_at: i64,
}

/// 把一个已提交的 [`TurnRecord`] 投影为产品 [`Turn`]。
///
/// `created_at` 是 Turn 的产品起始时间，`updated_at` 与 `revision` 是这条事实提交时的水位。
///
/// # Errors
/// core Turn 状态、阶段计数或最终时长无法投影时报错，不猜测占位值。
pub(crate) fn project_turn(
    thread_id: &str,
    snapshot: &CoreThreadSnapshot,
    record: &TurnRecord,
    created_at: i64,
    updated_at: i64,
    revision: u64,
) -> Result<Turn, ProjectionError> {
    let stamp = Stamp {
        started_at: created_at,
        updated_at,
    };
    Ok(Turn {
        input_id: record.input_id.clone(),
        id: record.turn_id.clone(),
        thread_id: thread_id.to_owned(),
        revision,
        state: state(snapshot, record, &stamp)?,
        updated_at,
    })
}

/// 投影当前仍在运行的 Turn（权威快照里唯一的 active Turn）；没有则返回 `None`。
///
/// core 的 [`TurnRecord`] 不保存起始时间，因此运行中的 Turn 以当前提交水位同时充当起始与更新
/// 时间；真正的阶段由 [`phase`] 从实时 attempts/tasks 重新派生。
///
/// # Errors
/// 运行中的 Turn 状态无法投影时报错。
pub(crate) fn project_active_turn(
    thread_id: &str,
    snapshot: &CoreThreadSnapshot,
    updated_at: i64,
) -> Result<Option<Turn>, ProjectionError> {
    let Some(record) = snapshot
        .turns
        .iter()
        .rev()
        .find(|record| record.state == CoreTurnState::Running)
    else {
        return Ok(None);
    };
    Ok(Some(project_turn(
        thread_id,
        snapshot,
        record,
        updated_at,
        updated_at,
        snapshot.commit_sequence,
    )?))
}

/// core Turn 状态 → 产品 Turn 状态。
fn state(
    snapshot: &CoreThreadSnapshot,
    record: &TurnRecord,
    stamp: &Stamp,
) -> Result<TurnState, ProjectionError> {
    let started = Some(stamp.started_at);
    let at = stamp.updated_at;
    Ok(match &record.state {
        CoreTurnState::Running => TurnState::Running(RunningTurnState::new(
            stamp.started_at,
            phase(snapshot, &record.turn_id),
        )),
        CoreTurnState::Finished(CoreTurnOutcome::Completed) => {
            TurnState::Completed(CompletedTurnState::new(started, at, TurnCompletion::Normal))
        }
        CoreTurnState::Finished(CoreTurnOutcome::WaitingInteraction) => TurnState::Completed(
            CompletedTurnState::new(started, at, TurnCompletion::InteractionRequested),
        ),
        CoreTurnState::Finished(CoreTurnOutcome::StepLimit) => {
            let tasks = snapshot
                .tasks
                .values()
                .filter(|task| task.turn_id == record.turn_id)
                .collect::<Vec<_>>();
            let usage = BudgetUsage {
                model_steps: record.model_steps,
                tool_calls: tasks.len().try_into().map_err(|_| ProjectionError::Count)?,
                wait_calls: tasks
                    .iter()
                    .filter(|task| task.tool_id == "wait")
                    .count()
                    .try_into()
                    .map_err(|_| ProjectionError::Count)?,
                elapsed_ms: record
                    .elapsed_ms
                    .ok_or_else(|| ProjectionError::MissingDuration(record.turn_id.clone()))?,
            };
            TurnState::BudgetLimited(BudgetLimitedTurnState::new(
                started,
                at,
                BudgetLimitSnapshot {
                    kind: BudgetLimitKind::ModelStep,
                    usage,
                },
                TurnRolloverOutcome::NotAttempted,
            ))
        }
        CoreTurnState::Cancelled => TurnState::Cancelled(CancelledTurnState::new(
            started,
            at,
            at,
            TurnCancellationCause::Unspecified,
        )),
        CoreTurnState::Interrupted => TurnState::Cancelled(CancelledTurnState::new(
            started,
            at,
            at,
            if record.elapsed_ms.is_some() {
                TurnCancellationCause::Interrupted
            } else {
                TurnCancellationCause::Recovery
            },
        )),
        CoreTurnState::Failed { description } => TurnState::Failed(FailedTurnState::new(
            started,
            at,
            failure(snapshot, &record.turn_id, description),
        )),
    })
}

/// 运行中 Turn 的 canonical 阶段，实时从 snapshot 的 typed 事实派生。
///
/// 阶段是运行中 tasks 与最新 attempt outcome 的投影，因此任务启动/结束或 attempt 提交时都会改
/// 变；这些事件都不会重写 Turn 记录本身，调用方必须在每个 effect 之后重新派生，而不是缓存 Turn
/// 记录里不存在的阶段。
fn phase(snapshot: &CoreThreadSnapshot, turn_id: &str) -> TurnPhase {
    if snapshot
        .tasks
        .values()
        .any(|task| task.turn_id == turn_id && task.status == TaskStatus::Running)
    {
        return TurnPhase::RunningTool;
    }
    match snapshot
        .attempts
        .iter()
        .rev()
        .find(|attempt| attempt.turn_id == turn_id)
        .map(|attempt| &attempt.outcome)
    {
        None => TurnPhase::Preparing,
        Some(AttemptOutcome::Committed(output)) if !output.tool_calls.is_empty() => {
            TurnPhase::Planning
        }
        Some(AttemptOutcome::Committed(_)) => TurnPhase::Responding,
        Some(
            AttemptOutcome::Running
            | AttemptOutcome::Interrupted
            | AttemptOutcome::Cancelled { .. }
            | AttemptOutcome::Failed(_)
            | AttemptOutcome::Rejected { .. },
        ) => TurnPhase::Thinking,
    }
}

/// 从最新失败 attempt 的 typed receipt 投影结构化失败；缺失或无法解码时显式降级。
fn failure(snapshot: &CoreThreadSnapshot, turn_id: &str, description: &str) -> TurnFailure {
    let Some(error) = snapshot
        .attempts
        .iter()
        .rev()
        .filter(|attempt| attempt.turn_id == turn_id)
        .find_map(|attempt| match &attempt.outcome {
            AttemptOutcome::Failed(error) => Some(error),
            AttemptOutcome::Running
            | AttemptOutcome::Interrupted
            | AttemptOutcome::Committed(_)
            | AttemptOutcome::Cancelled { .. }
            | AttemptOutcome::Rejected { .. } => None,
        })
    else {
        return TurnFailure::permanent(TurnFailureCategory::Internal, description);
    };
    match pl_model::runtime::model_failure_receipt(error) {
        Ok(Some(receipt)) => match receipt.provider_failure {
            Some(failure) => TurnFailure {
                category: if failure.kind == ProviderFailureKind::Capacity {
                    TurnFailureCategory::ProviderCapacity
                } else {
                    TurnFailureCategory::Provider
                },
                provider_kind: Some(failure.kind),
                code: failure.code,
                http_status: failure.http_status,
                message: failure.message,
                retry: failure.retry,
            },
            None => TurnFailure::permanent(
                TurnFailureCategory::Provider,
                if receipt.message.is_empty() {
                    description.to_owned()
                } else {
                    receipt.message
                },
            ),
        },
        Ok(None) => TurnFailure::permanent(TurnFailureCategory::Provider, description),
        Err(error) => TurnFailure::permanent(
            TurnFailureCategory::Protocol,
            format!("{description}; saved model failure cannot be decoded: {error}"),
        ),
    }
}
