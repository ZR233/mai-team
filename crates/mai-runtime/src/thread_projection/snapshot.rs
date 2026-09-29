//! 权威 Thread 快照、typed 存储状态、pending interaction 与累计 usage 的只读投影。
//!
//! 所有输入都是 PL core 的 typed 事实；产品元数据（Mode、工作区、标题、角色）来自 mai。任何
//! core 事实都无法投影时返回 [`ProjectionError`]，不填占位值，也不从错误文本推断类别。

use mai_protocol::{
    AgentSummary, Thread, ThreadRuntimeSnapshot, ThreadRuntimeUsage, ThreadSnapshot, ThreadStatus,
};
use pl_core::thread::{
    ThreadLifecycle, ThreadSnapshot as CoreThreadSnapshot, TurnState as CoreTurnState, UsageCost,
    UsageSummary,
    cold::{PersistenceState, StorageExecutionPhase, StorageFaultKind},
    input::{InputExecution, InputState},
    interactions::{InteractionRecord, InteractionState},
    permissions::PermissionState,
};
use pl_protocol::studio::HistoryFault;
use pl_protocol::{
    CacheUsageSummary, InteractionRequest, InteractionScope, RuntimeCostAmount, ThreadModeId,
    ThreadStorageExecution, ThreadStorageState, ThreadWorkspaceMode, UserQuestion,
};

use super::{ProjectionError, turns::project_active_turn};

/// PL 内建 `request_user_input` 工具写入 user-input interaction 的 payload 格式。
///
/// 内容是一份 `Vec<UserQuestion>` JSON。mai 只解码这一种 PL 拥有的编码，不定义也不猜测其它
/// 格式。
const USER_INPUT_FORMAT: &str = "pl.tool.user-input";

/// mai 拥有的 Thread 元数据；不来自 PL core，也不由 PL 管理。
pub(crate) struct ThreadProjectionMetadata<'a> {
    /// 产品 Thread 的 canonical Mode（如 `mode.simple` / `mode.task` / `mode.review`）。
    pub mode: ThreadModeId,
    /// 会话工作区模式；mai 的 canonical 产品事实。
    pub workspace_mode: ThreadWorkspaceMode,
    /// 会话工作区地址；mai 的 canonical 产品事实。
    pub workspace_path: &'a str,
    /// Thread 装配时冻结的 MCP generation 所含服务。
    pub active_mcp_servers: &'a [String],
}

/// 投影一个 Thread 的权威首帧。
///
/// `state` 是该 Thread 当前驻留的 canonical 事实；`summary` 提供产品身份与元数据。`revision`
/// 取 core 的 `commit_sequence`，`storage` 直接来自 core 的 typed 持久化状态。快照的首帧没有上
/// 一条活动，因此不携带任何产品侧的增量水位。
///
/// # Errors
/// 当存在 mai 无法投影的 interaction/permission 事实时返回 [`ProjectionError`]。
pub(crate) fn project_snapshot(
    summary: &AgentSummary,
    state: &CoreThreadSnapshot,
    metadata: &ThreadProjectionMetadata<'_>,
) -> Result<ThreadSnapshot, ProjectionError> {
    let thread_id = summary.id.to_string();
    let thread = project_thread_metadata(summary, state, metadata);
    let updated_at = thread.updated_at;
    Ok(ThreadSnapshot {
        schema_version: pl_protocol::THREAD_SCHEMA_VERSION,
        revision: state.commit_sequence,
        thread,
        active_turn: project_active_turn(&thread_id, state, updated_at)?,
        interactions: project_interactions(&thread_id, state)?,
        runtime: Some(project_runtime(
            &thread_id,
            state,
            updated_at,
            metadata.active_mcp_servers,
        )?),
        // 产品活动摘要（typed activity）是另一条独立投影，不在这里伪造。
        activity: None,
        storage: Some(storage_state(&state.persistence)),
    })
}

/// 用 mai 的产品元数据和一个 canonical core 快照绑定出产品 [`Thread`]。
///
/// `status` 来自 core 事实，`archived` 只由 `Closed` 生命周期决定；其余身份、角色、标题与 Mode
/// 都来自产品侧，绝不回读 PL 的旧 Agent 快照。
pub(crate) fn project_thread_metadata(
    summary: &AgentSummary,
    state: &CoreThreadSnapshot,
    metadata: &ThreadProjectionMetadata<'_>,
) -> Thread {
    let id = summary.id.to_string();
    let parent = summary.parent_id.map(|parent| parent.to_string());
    Thread {
        id: id.clone(),
        project_id: summary
            .project_id
            .map(|project| project.to_string())
            .or_else(|| summary.task_id.map(|task| task.to_string()))
            .unwrap_or_default(),
        title: summary.name.clone(),
        mode: metadata.mode.clone(),
        workspace_mode: metadata.workspace_mode,
        workspace_path: metadata.workspace_path.to_owned(),
        root_thread_id: parent.clone().unwrap_or_else(|| id.clone()),
        parent_thread_id: parent,
        role: summary
            .role
            .map(|role| role.to_string())
            .unwrap_or_default(),
        agent_path: summary.name.clone(),
        status: status(state),
        created_at: summary.created_at.timestamp(),
        updated_at: summary.updated_at.timestamp(),
        archived: state.lifecycle == ThreadLifecycle::Closed,
    }
}

/// Thread 的产品状态，按 core 事实的唯一顺序判定。
///
/// 顺序是 canonical 的：生命周期 > 取消/故障 > 未决 interaction/permission > 运行中的 Turn >
/// 待执行 input > 空闲。它不返回 `WaitingTool`：工具执行由运行中 Turn 的 phase 表达，产品状态
/// 层不额外区分。
pub(crate) fn status(state: &CoreThreadSnapshot) -> ThreadStatus {
    match state.lifecycle {
        ThreadLifecycle::Closing => ThreadStatus::Closing,
        ThreadLifecycle::Closed => ThreadStatus::Closed,
        ThreadLifecycle::Open => {
            if matches!(state.input_execution, InputExecution::Interrupting { .. }) {
                return ThreadStatus::Cancelling;
            }
            if matches!(state.input_execution, InputExecution::Failed { .. }) {
                return ThreadStatus::Faulted;
            }
            if state
                .interactions
                .values()
                .any(|record| record.state == InteractionState::Pending)
                || state
                    .permissions
                    .values()
                    .any(|record| record.state == PermissionState::Pending)
            {
                ThreadStatus::WaitingInteraction
            } else if state
                .turns
                .iter()
                .any(|turn| turn.state == CoreTurnState::Running)
            {
                ThreadStatus::Running
            } else if state
                .inputs
                .iter()
                .any(|input| input.state == InputState::Pending)
            {
                ThreadStatus::Queued
            } else {
                ThreadStatus::Idle
            }
        }
    }
}

/// core 的 typed 持久化状态 → 产品存储状态。
///
/// 故障类别与水位全部来自 core 的 typed 值；`last_error` 只是诊断文本，绝不参与类别推断。未挂
/// 载存储时水位保持 `None`（未知），不写成 0。
pub(crate) fn storage_state(persistence: &PersistenceState) -> ThreadStorageState {
    ThreadStorageState {
        fault: persistence.fault.map(storage_fault),
        fault_generation: persistence.fault_generation,
        accepted_sequence: persistence
            .attached
            .then_some(persistence.admitted_sequence),
        durable_sequence: persistence.attached.then_some(persistence.durable_sequence),
        execution: match persistence.execution_phase {
            StorageExecutionPhase::Running => ThreadStorageExecution::Running,
            StorageExecutionPhase::PausingForStorage => ThreadStorageExecution::Pausing,
            StorageExecutionPhase::PausedForStorage => ThreadStorageExecution::Paused,
        },
        pressure_paused: persistence.pressure_paused,
        // 显式继续闩是 core 的安全点事实，不是从故障类别猜出来的。
        resume_required: persistence.resume_required,
        can_resume: persistence.resume_ready,
        last_error: persistence.error.clone(),
    }
}

/// 从 core 的累计 [`UsageSummary`] 投影产品 runtime 用量。
///
/// 总量来自 Thread 的累计摘要，而不是仍驻留的 attempts：已完成的 attempt 会被提交丢弃，若从
/// attempts 重新聚合，热读、重连与冷读会互相不一致。产品模型路由（provider/model）属于 mai 配
/// 置，见 [`AgentSummary`]，这里不复制成 PL route 快照、也不伪造 route revision，因此
/// `model_route` 保持 `None`。
pub(crate) fn project_runtime(
    thread_id: &str,
    state: &CoreThreadSnapshot,
    updated_at: i64,
    active_mcp_servers: &[String],
) -> Result<ThreadRuntimeSnapshot, ProjectionError> {
    let summary: &UsageSummary = &state.usage_summary;
    let active_skills = state
        .extensions
        .values()
        .filter(|record| record.payload.format() == "pl.tool.skill-view")
        .map(|record| {
            pl_tool::skill::saved_skill_name(&record.payload)
                .map_err(|error| ProjectionError::Skill(error.to_string()))
        })
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?
        .into_iter()
        .collect();
    Ok(ThreadRuntimeSnapshot {
        thread_id: thread_id.to_owned(),
        model_route: None,
        usage: ThreadRuntimeUsage {
            has_incomplete_usage: summary.has_incomplete_usage,
            model: summary.model.clone(),
            context_window: summary.context_window,
            latest_context_tokens: summary.latest_context_tokens,
            prompt_tokens: summary.prompt_tokens,
            completion_tokens: summary.completion_tokens,
            cached_prompt_tokens: summary.cached_prompt_tokens,
            cache_write_tokens: summary.cache_write_tokens,
            reasoning_tokens: summary.reasoning_tokens,
            inference_count: summary.inference_count,
            total_tokens: summary.total_tokens,
            cache_usage: CacheUsageSummary {
                input_tokens: summary.cache_input_tokens,
                cache_read_tokens: summary.cache_read_tokens,
                hit_rate: (summary.cache_input_tokens > 0)
                    .then(|| summary.cache_read_tokens as f64 / summary.cache_input_tokens as f64),
                has_incomplete_usage: summary.cache_incomplete,
            },
            estimated_costs: runtime_costs(&summary.estimated_costs),
            estimated_cache_savings: runtime_costs(&summary.estimated_cache_savings),
            has_unpriced_usage: summary.has_unpriced_usage,
            prompt_generation: None,
            prompt_cache_policy: None,
            prefix_changed_reason: None,
            updated_at,
        },
        turn_completion_tokens: summary.turn_completion_tokens,
        turn_decode_millis: summary.turn_decode_millis,
        // Skill 激活由 pl-tool 保存的 typed extension 投影；其它活动摘要仍各自独立。
        todo: None,
        active_skills,
        active_mcp_servers: active_mcp_servers.to_vec(),
        active_lsp_servers: Vec::new(),
        progress: None,
        mcp_health: None,
        workflow: None,
        updated_at,
    })
}

/// 投影 Thread 当前仍未决、需要产品回答的 interaction。
///
/// core 的常驻 `interactions` 只保留未决记录，因此这里不丢弃任何待回答事实；由于 mai 没有工具
/// 审批 payload 契约，任何未决 Permission 都无法投影，必须显式报错而不是让快照看起来已空闲。
///
/// # Errors
/// 存在 Permission、interaction identity 不一致、记录非 pending，或 payload 不是已知格式时报错。
pub(crate) fn project_interactions(
    thread_id: &str,
    state: &CoreThreadSnapshot,
) -> Result<Vec<InteractionRequest>, ProjectionError> {
    if let Some((permission_id, _)) = state.permissions.iter().next() {
        return Err(ProjectionError::UnsupportedPermission {
            thread_id: thread_id.to_owned(),
            permission_id: permission_id.clone(),
        });
    }
    state
        .interactions
        .iter()
        .map(|(id, record)| project_user_input(thread_id, id, record))
        .collect()
}

/// 投影一条 user-input interaction，保留 canonical 身份、revision 与时间。
fn project_user_input(
    thread_id: &str,
    interaction_id: &str,
    record: &InteractionRecord,
) -> Result<InteractionRequest, ProjectionError> {
    if record.request.id != interaction_id {
        return Err(ProjectionError::InteractionIdentity(
            interaction_id.to_owned(),
        ));
    }
    if record.state != InteractionState::Pending {
        return Err(ProjectionError::InteractionNotPending(
            interaction_id.to_owned(),
        ));
    }
    let payload = &record.request.payload;
    if payload.format() != USER_INPUT_FORMAT || payload.version() != 1 {
        return Err(ProjectionError::UnsupportedInteraction {
            format: payload.format().to_owned(),
            version: payload.version(),
        });
    }
    let questions: Vec<UserQuestion> = serde_json::from_str(payload.content())?;
    let mut projected = InteractionRequest::user_input(
        interaction_id.to_owned(),
        InteractionScope {
            thread_id: thread_id.to_owned(),
            turn_id: record.request.turn_id.clone(),
            item_id: None,
            tool_id: None,
            agent_path: Some(thread_id.to_owned()),
            purpose: Default::default(),
        },
        questions,
        record.created_at,
    );
    projected.revision = record.revision;
    projected.updated_at = record.updated_at;
    Ok(projected)
}

/// core 的 typed 存储故障 → 产品故障类别。
fn storage_fault(fault: StorageFaultKind) -> HistoryFault {
    match fault {
        StorageFaultKind::QueueFull => HistoryFault::QueueFull,
        StorageFaultKind::WriteFailed => HistoryFault::WriteFailed,
        StorageFaultKind::WriterUnavailable => HistoryFault::WriterUnavailable,
        StorageFaultKind::NoProgress => HistoryFault::NoProgress,
        StorageFaultKind::CheckpointFailed => HistoryFault::CheckpointFailed,
        StorageFaultKind::BlobFailed => HistoryFault::BlobFailed,
    }
}

/// core 的估算成本 → 产品运行时成本。
fn runtime_costs(costs: &[UsageCost]) -> Vec<RuntimeCostAmount> {
    costs
        .iter()
        .map(|cost| RuntimeCostAmount {
            currency: cost.currency.clone(),
            amount: cost.amount,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use pl_core::context::OpaquePayload;
    use pl_core::thread::extensions::ExtensionRecord;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    #[test]
    fn runtime_projects_active_skills_and_frozen_mcp_servers() {
        let payload = OpaquePayload::new(
            "pl.tool.skill-view",
            1,
            json!({
                "success": true,
                "skill": {
                    "name": "rust-code-quality",
                    "description": "Rust review rules",
                    "category": null,
                    "platforms": [],
                    "source": "project",
                    "providerId": "mai-filesystem-skills",
                    "invocation": {
                        "modelInvocable": true,
                        "userInvocable": true
                    },
                    "resourceBase": {
                        "kind": "directory",
                        "path": "/project/repo/.agents/skills/rust-code-quality"
                    }
                },
                "filePath": "SKILL.md",
                "resourceBase": {
                    "kind": "directory",
                    "path": "/project/repo/.agents/skills/rust-code-quality"
                },
                "resourceHint": "Use filePath to read support resources on demand.",
                "content": "Review Rust code."
            })
            .to_string(),
        )
        .expect("valid typed Skill receipt");
        let mut state = CoreThreadSnapshot::default();
        state.extensions.insert(
            "pl.tool.skill-view:rust-code-quality".to_owned(),
            ExtensionRecord {
                revision: 7,
                payload,
            },
        );

        let runtime = project_runtime(
            "thread-a",
            &state,
            1_700_000_000,
            &["zhipu_search".to_string()],
        )
        .expect("project runtime with typed Skill extension");

        assert_eq!(runtime.active_skills, vec!["rust-code-quality"]);
        assert_eq!(runtime.active_mcp_servers, vec!["zhipu_search"]);
    }
}
