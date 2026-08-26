use std::sync::Arc;

use mai_protocol::{AgentSummary, Thread, ThreadMode, ThreadStatus, ThreadTextChannel, TokenUsage};
use mai_store::{MaiStore, StoredThreadRuntime};
use pl_core::{AgentSnapshot, AgentState, ThreadId};

use crate::{Result, RuntimeError};

/// 产品 AgentId 即 PL ThreadId，边界只做非空校验。
pub(crate) fn canonical_id(agent_id: mai_protocol::AgentId) -> Result<pl_core::ThreadId> {
    pl_core::ThreadId::new(agent_id.to_string()).map_err(RuntimeError::Model)
}

/// 加载一个 Agent 唯一拥有的 canonical Thread document。
pub(crate) async fn load_runtime(
    store: &Arc<MaiStore>,
    thread_id: &ThreadId,
) -> Result<StoredThreadRuntime> {
    store
        .load_thread_runtime(thread_id.as_str())
        .await?
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "canonical Thread state is missing for `{thread_id}`"
            ))
        })
}

/// 从 canonical Thread runtime usage 构造产品汇总。
pub(crate) fn aggregate_usage(runtime: &StoredThreadRuntime) -> TokenUsage {
    let Some(usage) = runtime
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.runtime.as_ref())
        .map(|runtime| &runtime.usage)
    else {
        return TokenUsage::default();
    };
    TokenUsage {
        prompt_tokens: usage.prompt_tokens,
        cached_prompt_tokens: usage.cached_prompt_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        completion_tokens: usage.completion_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        total_tokens: usage.total_tokens,
    }
}

/// 返回最近一次 final agent message。
pub(crate) fn last_agent_response(runtime: &StoredThreadRuntime) -> Option<String> {
    runtime
        .snapshot
        .as_ref()?
        .items
        .iter()
        .rev()
        .find_map(|item| {
            let text = item.text()?;
            (text.channel() == ThreadTextChannel::Final).then(|| text.text().to_string())
        })
}

/// 将产品 Agent metadata 绑定到 PL Thread snapshot。
pub(crate) fn thread_metadata(summary: &AgentSummary, snapshot: &AgentSnapshot) -> Thread {
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
        mode: if summary.task_id.is_some() || summary.project_id.is_some() {
            ThreadMode::Task
        } else {
            ThreadMode::Simple
        },
        root_thread_id: parent.clone().unwrap_or_else(|| id.clone()),
        parent_thread_id: parent,
        role: summary
            .role
            .map(|role| role.to_string())
            .unwrap_or_default(),
        agent_path: summary.name.clone(),
        status: match &snapshot.state {
            AgentState::Idle(_) => ThreadStatus::Idle,
            AgentState::Queued(_) => ThreadStatus::Queued,
            AgentState::Running(_) => ThreadStatus::Running,
            AgentState::WaitingTool(_) => ThreadStatus::WaitingTool,
            AgentState::WaitingInteraction(_) => ThreadStatus::WaitingInteraction,
            AgentState::Cancelling(_) => ThreadStatus::Cancelling,
            AgentState::Closing(_) => ThreadStatus::Closing,
            AgentState::Closed(_) => ThreadStatus::Closed,
            AgentState::Faulted(_) => ThreadStatus::Faulted,
        },
        created_at: summary.created_at.timestamp(),
        updated_at: snapshot.updated_at,
        archived: matches!(snapshot.state, AgentState::Closed(_)),
    }
}
