use std::sync::{Arc, Weak};

use mai_protocol::MaiProductEventKind;
use pl_core::{
    AgentCommitObserver, AgentCommittedEvent, AgentLifecycleState, AgentRuntimeEventKind,
    AgentSnapshot, TurnOutcomeKind,
};

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
        session_id,
        turn_id,
        runtime_events,
        trace_events,
        session_events: _,
    } = committed;
    if !trace_events.is_empty() {
        let session_id = session_id.ok_or_else(|| {
            crate::RuntimeError::InvalidInput(
                "durable trace batch is missing session id".to_string(),
            )
        })?;
        let turn_id = turn_id.ok_or_else(|| {
            crate::RuntimeError::InvalidInput("durable trace batch is missing turn id".to_string())
        })?;
        let (product_agent_id, _) = super::turn_factory::product_agent(runtime, &agent_id).await?;
        super::trace_projection::project_trace_events(
            runtime,
            product_agent_id,
            super::protocol_uuid(session_id.as_str()),
            super::protocol_uuid(turn_id.as_str()),
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
        | AgentRuntimeEventKind::SessionOpened { snapshot, .. } => {
            persist_state(runtime, snapshot).await?;
        }
        AgentRuntimeEventKind::TurnStarted {
            turn_id,
            session_id,
            snapshot,
        } => {
            let agent_id = persist_state(runtime, snapshot).await?;
            super::trace_projection::record_agent_log(
                runtime,
                super::trace_projection::AgentLogProjection {
                    agent_id,
                    session_id: Some(super::protocol_uuid(session_id.as_str())),
                    turn_id: Some(super::protocol_uuid(turn_id.as_str())),
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
            let agent_id = persist_state(runtime, snapshot).await?;
            let session_id = super::protocol_uuid(outcome.session_id.as_str());
            let turn_id = super::protocol_uuid(outcome.turn_id.as_str());
            super::trace_projection::record_agent_log(
                runtime,
                super::trace_projection::AgentLogProjection {
                    agent_id,
                    session_id: Some(session_id),
                    turn_id: Some(turn_id),
                    level: match outcome.kind {
                        TurnOutcomeKind::Completed | TurnOutcomeKind::Cancelled => "info",
                        TurnOutcomeKind::Failed | TurnOutcomeKind::BudgetLimited => "warn",
                    },
                    category: "turn",
                    message: "turn completed",
                    details: serde_json::json!({
                        "outcome": outcome.kind,
                        "reason": outcome.reason,
                        "revision": event.sequence,
                    }),
                    timestamp: event_time,
                },
            )
            .await;
        }
        AgentRuntimeEventKind::Faulted { reason, snapshot } => {
            let agent_id = persist_state(runtime, snapshot).await?;
            super::trace_projection::record_agent_log(
                runtime,
                super::trace_projection::AgentLogProjection {
                    agent_id,
                    session_id: None,
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

/// 持久化 PL runtime 的兼容产品投影，但不把高频 session/turn 状态重新广播为产品事件。
///
/// 当前会话的 UI 状态只由 PL session stream 驱动；产品事件仅用于 agent 资源、配置等
/// 低频变化，避免每个 turn transition 都触发 AgentDetail 和项目/任务查询失效。
async fn persist_state(
    runtime: &Arc<AgentRuntime>,
    snapshot: AgentSnapshot,
) -> crate::Result<mai_protocol::AgentId> {
    let (agent_id, _) = project_state(runtime, snapshot).await?;
    Ok(agent_id)
}

/// 启动恢复后以 PL snapshot 覆盖产品内存投影，不额外制造状态变更事件。
pub(crate) async fn synchronize_runtime_state(
    runtime: &Arc<AgentRuntime>,
    snapshot: AgentSnapshot,
) -> crate::Result<()> {
    project_state(runtime, snapshot).await.map(|_| ())
}

async fn project_state(
    runtime: &Arc<AgentRuntime>,
    snapshot: AgentSnapshot,
) -> crate::Result<(mai_protocol::AgentId, mai_protocol::AgentSummary)> {
    let (agent_id, agent) =
        super::turn_factory::product_agent(runtime, &snapshot.identity.id).await?;
    let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
    let canonical = super::load_runtime(&runtime.deps.store, &runtime_agent_id).await?;
    let summary = {
        let mut summary = agent.summary.write().await;
        summary.state.runtime = super::runtime_state(&snapshot);
        summary.token_usage = super::aggregate_usage(&canonical);
        match snapshot.lifecycle {
            AgentLifecycleState::Closing => {
                summary.state.resource = mai_protocol::AgentResourceState::Deleting;
            }
            AgentLifecycleState::Closed => {
                summary.state.resource = mai_protocol::AgentResourceState::Deleted;
                summary.state.resource_error = None;
            }
            AgentLifecycleState::Active | AgentLifecycleState::Faulted => {}
        }
        summary.updated_at = chrono::DateTime::from_timestamp(snapshot.updated_at, 0)
            .unwrap_or_else(chrono::Utc::now);
        summary.clone()
    };
    runtime
        .deps
        .store
        .save_agent_with_runtime_id(
            &summary,
            agent.system_prompt.as_deref(),
            runtime_agent_id.as_str(),
        )
        .await?;
    Ok((agent_id, summary))
}
