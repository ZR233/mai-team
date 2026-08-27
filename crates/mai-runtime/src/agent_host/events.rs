use std::sync::{Arc, Weak};

use mai_protocol::MaiProductEventKind;
use pl_core::{
    AgentCommitObserver, AgentCommittedEvent, AgentRuntimeEventKind, AgentSnapshot, AgentState,
};
use pl_protocol::TurnOutcome;

use crate::AgentRuntime;

/// 将已持久化的 PL event 投影到 mai 产品状态和只读观测记录。
#[derive(Clone)]
pub(crate) struct MaiAgentCommitObserver {
    runtime: Weak<AgentRuntime>,
}

impl MaiAgentCommitObserver {
    pub(crate) fn new(runtime: Weak<AgentRuntime>) -> Self {
        Self { runtime }
    }
}

impl AgentCommitObserver for MaiAgentCommitObserver {
    async fn publish(&self, committed: AgentCommittedEvent) {
        let Some(runtime) = self.runtime.upgrade() else {
            return;
        };
        if let Err(error) = project_event(&runtime, committed).await {
            tracing::warn!("failed to project durable PL agent event: {error}");
        }
    }
}

async fn project_event(
    runtime: &Arc<AgentRuntime>,
    committed: AgentCommittedEvent,
) -> crate::Result<()> {
    let AgentCommittedEvent {
        agent_id,
        thread_id,
        turn_id,
        runtime_events,
        trace_events,
        thread_notifications: _,
    } = committed;
    if !trace_events.is_empty() {
        let thread_id = thread_id.ok_or_else(|| {
            crate::RuntimeError::InvalidInput(
                "durable trace batch is missing thread id".to_string(),
            )
        })?;
        let turn_id = turn_id.ok_or_else(|| {
            crate::RuntimeError::InvalidInput("durable trace batch is missing turn id".to_string())
        })?;
        let (product_agent_id, _) = super::turn_factory::product_agent(runtime, &agent_id).await?;
        super::trace_projection::project_trace_events(
            runtime,
            product_agent_id,
            thread_id.to_string(),
            turn_id.to_string(),
            &trace_events,
        )
        .await;
    }
    for event in runtime_events {
        project_runtime_event(runtime, event).await?;
    }
    Ok(())
}

async fn project_runtime_event(
    runtime: &Arc<AgentRuntime>,
    event: pl_core::AgentRuntimeEvent,
) -> crate::Result<()> {
    let event_time = super::trace_projection::trace_time(event.created_at);
    match event.kind {
        AgentRuntimeEventKind::Registered { snapshot }
        | AgentRuntimeEventKind::StateChanged { snapshot }
        | AgentRuntimeEventKind::TurnQueued { snapshot, .. }
        | AgentRuntimeEventKind::ThreadOpened { snapshot, .. }
        | AgentRuntimeEventKind::TurnActivityChanged { snapshot, .. } => {
            persist_state(runtime, *snapshot).await?;
        }
        AgentRuntimeEventKind::TurnStarted {
            turn_id,
            thread_id,
            input: _,
            claimed_inputs: _,
            snapshot,
        } => {
            let agent_id = persist_state(runtime, *snapshot).await?;
            super::trace_projection::record_agent_log(
                runtime,
                super::trace_projection::AgentLogProjection {
                    agent_id,
                    thread_id: Some(thread_id.to_string()),
                    turn_id: Some(turn_id.to_string()),
                    level: "info",
                    category: "turn",
                    message: "turn started",
                    details: serde_json::json!({ "revision": event.sequence }),
                    timestamp: event_time,
                },
            )
            .await;
        }
        AgentRuntimeEventKind::TurnFinished { outcome, snapshot }
        | AgentRuntimeEventKind::RecoveryCancelledTurn { outcome, snapshot } => {
            let agent_id = persist_state(runtime, *snapshot).await?;
            super::trace_projection::record_agent_log(
                runtime,
                super::trace_projection::AgentLogProjection {
                    agent_id,
                    thread_id: Some(outcome.thread_id.to_string()),
                    turn_id: Some(outcome.turn_id.to_string()),
                    level: match &outcome.outcome {
                        TurnOutcome::Completed(_) | TurnOutcome::Cancelled(_) => "info",
                        TurnOutcome::Failed(_) | TurnOutcome::BudgetLimited(_) => "warn",
                    },
                    category: "turn",
                    message: "turn completed",
                    details: serde_json::json!({
                        "outcome": outcome.outcome,
                        "revision": event.sequence,
                    }),
                    timestamp: event_time,
                },
            )
            .await;
        }
        AgentRuntimeEventKind::Faulted { reason, snapshot } => {
            let agent_id = persist_state(runtime, *snapshot).await?;
            super::trace_projection::record_agent_log(
                runtime,
                super::trace_projection::AgentLogProjection {
                    agent_id,
                    thread_id: None,
                    turn_id: None,
                    level: "error",
                    category: "runtime",
                    message: "agent faulted",
                    details: serde_json::json!({
                        "reason": reason,
                        "revision": event.sequence
                    }),
                    timestamp: event_time,
                },
            )
            .await;
            runtime
                .events
                .publish(MaiProductEventKind::OperationFailed {
                    scope: "agent_runtime".to_string(),
                    agent_id: Some(agent_id),
                    message: reason,
                })
                .await;
        }
    }
    Ok(())
}

/// 持久化 PL runtime 的产品资源投影，但不把高频 Thread/Turn 状态重新广播为产品事件。
///
/// 当前 Thread 的 UI 状态只由 PL subscription 驱动；产品事件仅用于 agent 资源、配置等
/// 低频变化，避免每个 turn transition 都触发 AgentDetail 和项目/任务查询失效。
async fn persist_state(
    runtime: &AgentRuntime,
    snapshot: AgentSnapshot,
) -> crate::Result<mai_protocol::AgentId> {
    let (agent_id, _) = project_state(runtime, snapshot).await?;
    Ok(agent_id)
}

/// 启动恢复后以 PL snapshot 覆盖产品内存投影，不额外制造状态变更事件。
pub(crate) async fn synchronize_runtime_state(
    runtime: &AgentRuntime,
    snapshot: AgentSnapshot,
) -> crate::Result<()> {
    project_state(runtime, snapshot).await.map(|_| ())
}

async fn project_state(
    runtime: &AgentRuntime,
    snapshot: AgentSnapshot,
) -> crate::Result<(mai_protocol::AgentId, mai_protocol::AgentSummary)> {
    let (agent_id, agent) =
        super::turn_factory::product_agent(runtime, &snapshot.identity.id).await?;
    let thread_id = super::canonical_id(agent_id)?;
    let durable_runtime = super::load_runtime(&runtime.deps.store, &thread_id).await?;
    let token_usage = super::aggregate_usage(&durable_runtime);
    let mut current = agent.summary.write().await;
    let summary = {
        let mut summary = current.clone();
        match &snapshot.state {
            AgentState::Closing(_) => {
                summary.resource.state = mai_protocol::AgentResourceState::Deleting;
            }
            AgentState::Closed(_) => {
                summary.resource.state = mai_protocol::AgentResourceState::Deleted;
                summary.resource.error = None;
            }
            AgentState::Idle(_)
            | AgentState::Queued(_)
            | AgentState::Running(_)
            | AgentState::WaitingTool(_)
            | AgentState::WaitingInteraction(_)
            | AgentState::Cancelling(_)
            | AgentState::Faulted(_) => {}
        }
        summary.updated_at = chrono::DateTime::from_timestamp(snapshot.updated_at, 0)
            .unwrap_or_else(chrono::Utc::now);
        summary.runtime = Some(snapshot);
        summary.token_usage = token_usage;
        summary
    };
    runtime
        .deps
        .store
        .save_agent(&summary, agent.system_prompt.as_deref())
        .await?;
    *current = summary.clone();
    Ok((agent_id, summary))
}
