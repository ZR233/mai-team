use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use chrono::{DateTime, Utc};
use mai_protocol::{
    AgentDetail, AgentId, AgentMessage, AgentRole, AgentSessionSummary, AgentStatus, AgentSummary,
    ContextUsage, MessageRole, ModelInputItem, ServiceEvent, ServiceEventKind, SessionId, TurnId,
    now,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::state::{AgentRecord, AgentSessionRecord};
use crate::{Result, RuntimeError};

mod container;
mod create;
mod delete;
mod files;
mod fork;
mod input;
mod model;
mod resources;
mod spawn;
mod update;
mod wait;

pub(crate) use container::{
    AgentContainerOps, AgentContainerStartRequest, AgentContainerStatusChange,
    AgentMcpStatusChange, ContainerSource, ensure_agent_container, ensure_agent_container_for_turn,
    ensure_agent_container_with_source,
};
pub(crate) use create::{AgentCreateOps, CreateAgentRecordContext, create_agent_record};
pub(crate) use delete::{
    AgentContainerDeleteRequest, AgentDeleteOps, AgentDeleteStatusChange, delete_agent,
};
pub(crate) use files::{AgentFileOps, download_file_tar, upload_file};
pub(crate) use fork::fork_agent_context;
#[cfg(test)]
pub(crate) use input::start_next_queued_input;
pub(crate) use input::{send_input_to_agent, start_next_queued_input_after_turn};
pub(crate) use model::normalize_reasoning_effort;
pub(crate) use resources::AgentResourceBroker;
pub(crate) use spawn::{
    AgentSpawnOps, SpawnChildAgentRequest, spawn_child_agent, spawn_task_role_agent,
};
pub(crate) use update::{AgentUpdateOps, update_agent};
pub(crate) use wait::{wait_agent, wait_agent_until_complete_with_cancel};

#[async_trait::async_trait]
pub(crate) trait AgentServiceOps: Send + Sync {
    async fn agent(&self, agent_id: AgentId) -> Result<Arc<AgentRecord>>;
    async fn save_agent_session(
        &self,
        agent_id: AgentId,
        session: &AgentSessionSummary,
    ) -> Result<()>;
    async fn persist_agent(&self, agent: &AgentRecord) -> Result<()>;
    async fn publish(&self, event: ServiceEventKind);
    async fn recent_events_for_agent(&self, agent_id: AgentId) -> Vec<ServiceEvent>;
    async fn provider_context_tokens(&self, provider_id: &str, model: &str) -> Option<u64>;
    async fn resolve_session_id(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> Result<SessionId>;
    async fn replace_agent_history(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        history: &[ModelInputItem],
    ) -> Result<()>;
    async fn append_agent_message(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        position: usize,
        message: &AgentMessage,
    ) -> Result<()>;
    async fn delete_agent_containers(
        &self,
        agent_id: AgentId,
        preferred_container_id: Option<String>,
    ) -> Result<Vec<String>>;
    async fn ensure_agent_container(
        &self,
        agent: &Arc<AgentRecord>,
        status: AgentStatus,
    ) -> Result<()>;
}

/// Supplies the turn-control operations needed to interrupt, queue, and start
/// conversational input for an agent without exposing the full runtime.
pub(crate) trait AgentInputOps: Send + Sync {
    fn cancel_agent_turn(
        &self,
        agent_id: AgentId,
        turn_id: TurnId,
    ) -> impl Future<Output = Result<()>> + Send;
    fn set_agent_status(
        &self,
        agent: &Arc<AgentRecord>,
        status: AgentStatus,
        error: Option<String>,
    ) -> impl Future<Output = Result<()>> + Send;
    fn spawn_turn(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
    );
}

pub(crate) async fn list_agents(agents: Vec<Arc<AgentRecord>>) -> Vec<AgentSummary> {
    let mut summaries = Vec::with_capacity(agents.len());
    for agent in agents {
        summaries.push(agent.summary.read().await.clone());
    }
    summaries.sort_by_key(|summary| summary.created_at);
    summaries
}

pub(crate) async fn get_agent(
    ops: &dyn AgentServiceOps,
    agent_id: AgentId,
    session_id: Option<SessionId>,
    auto_compact_threshold_percent: u64,
) -> Result<AgentDetail> {
    let agent = ops.agent(agent_id).await?;
    let summary = agent.summary.read().await.clone();
    let (sessions, selected_session_id, context_tokens_used, messages) = {
        let sessions = agent.sessions.lock().await;
        let selected_session = selected_session(&sessions, session_id).ok_or_else(|| {
            RuntimeError::SessionNotFound {
                agent_id,
                session_id: session_id.unwrap_or_default(),
            }
        })?;
        (
            sessions
                .iter()
                .map(|session| session.summary.clone())
                .collect(),
            selected_session.summary.id,
            selected_session.last_context_tokens.unwrap_or_default(),
            selected_session.messages.clone(),
        )
    };
    let context_usage = ops
        .provider_context_tokens(&summary.provider_id, &summary.model)
        .await
        .map(|context_tokens| ContextUsage {
            used_tokens: context_tokens_used,
            context_tokens,
            threshold_percent: auto_compact_threshold_percent,
        });
    let recent_events = ops.recent_events_for_agent(agent_id).await;
    Ok(AgentDetail {
        summary,
        sessions,
        selected_session_id,
        context_usage,
        messages,
        recent_events,
    })
}

pub(crate) async fn create_session(
    ops: &dyn AgentServiceOps,
    agent_id: AgentId,
) -> Result<AgentSessionSummary> {
    let agent = ops.agent(agent_id).await?;
    if agent.summary.read().await.task_id.is_some() {
        return Err(RuntimeError::InvalidInput(
            "task-owned agents use a single internal task session".to_string(),
        ));
    }
    let session = {
        let mut sessions = agent.sessions.lock().await;
        let session = next_chat_session_record(sessions.len());
        sessions.push(session.clone());
        session.summary
    };
    ops.save_agent_session(agent_id, &session).await?;
    Ok(session)
}

pub(crate) async fn prepare_turn(
    ops: &dyn AgentServiceOps,
    agent_id: AgentId,
) -> Result<(Arc<AgentRecord>, TurnId)> {
    let agent = ops.agent(agent_id).await?;
    let turn_id = Uuid::new_v4();
    let should_start = {
        let mut summary = agent.summary.write().await;
        if !summary.status.can_start_turn() {
            false
        } else {
            summary.status = AgentStatus::RunningTurn;
            summary.current_turn = Some(turn_id);
            summary.updated_at = now();
            summary.last_error = None;
            agent.cancel_requested.store(false, Ordering::SeqCst);
            true
        }
    };
    if !should_start {
        return Err(RuntimeError::AgentBusy(agent_id));
    }
    ops.persist_agent(&agent).await?;
    ops.publish(ServiceEventKind::AgentStatusChanged {
        agent_id,
        status: AgentStatus::RunningTurn,
    })
    .await;
    Ok((agent, turn_id))
}

pub(crate) async fn close_agent(
    ops: &dyn AgentServiceOps,
    agent_id: AgentId,
) -> Result<AgentStatus> {
    let agent = ops.agent(agent_id).await?;
    agent.cancel_requested.store(true, Ordering::SeqCst);
    let previous_status = agent.summary.read().await.status.clone();
    if let Some(manager) = agent.mcp.write().await.take() {
        manager.shutdown().await;
    }
    let in_memory_container_id = agent
        .container
        .write()
        .await
        .take()
        .map(|container| container.id);
    let persisted_container_id = agent.summary.read().await.container_id.clone();
    let preferred_container_id = in_memory_container_id.or(persisted_container_id);
    ops.delete_agent_containers(agent_id, preferred_container_id)
        .await?;
    {
        let mut summary = agent.summary.write().await;
        summary.status = AgentStatus::Deleted;
        summary.container_id = None;
        summary.current_turn = None;
        summary.updated_at = now();
    }
    ops.persist_agent(&agent).await?;
    ops.publish(ServiceEventKind::AgentStatusChanged {
        agent_id,
        status: AgentStatus::Deleted,
    })
    .await;
    Ok(previous_status)
}

pub(crate) async fn resume_agent(
    ops: &dyn AgentServiceOps,
    agent_id: AgentId,
) -> Result<AgentSummary> {
    let agent = ops.agent(agent_id).await?;
    {
        let mut summary = agent.summary.write().await;
        if summary.status == AgentStatus::Deleted {
            summary.status = AgentStatus::Idle;
            summary.last_error = None;
            summary.updated_at = now();
        }
        summary.container_id = None;
    }
    ops.persist_agent(&agent).await?;
    ops.ensure_agent_container(&agent, AgentStatus::Idle)
        .await?;
    Ok(agent.summary.read().await.clone())
}

pub(crate) const PLANNER_SYSTEM_PROMPT: &str = r#"You are the Planner for a Mai task. Your job is to create a decision-complete implementation plan through a structured 3-phase process. A decision-complete plan can be handed to the Executor agent and implemented without any additional design decisions.

## 3-Phase Planning Process

### Phase 1 — Explore (discover facts, eliminate unknowns)
- Use `spawn_agent` with role `explorer` to investigate code, docs, and relevant context.
- Run read-only commands to understand the codebase structure, existing patterns, and constraints.
- Do NOT ask the user questions that can be answered by exploring the code.
- Only ask clarifying questions about the prompt if there are obvious ambiguities.

### Phase 2 — Intent Chat (clarify what they want)
- Use `request_user_input` to ask structured questions about: goal + success criteria, scope, constraints, and key preferences/tradeoffs.
- Each question must materially change the plan, confirm an assumption, or choose between meaningful tradeoffs.
- Offer 2-4 clear options with a recommended default.
- Bias toward asking over guessing when high-impact ambiguity remains.

### Phase 3 — Implementation Spec (produce the plan)
- Create a complete implementation specification covering: approach, interfaces/data flow, edge cases, testing strategy, and assumptions.
- The plan must be decision-complete — the Executor should not need to make any design decisions.

## Rules

- **No code modification**: Only explore and plan. Never edit files or make changes.
- **Use `save_task_plan`** to save or update the plan with a clear title and complete Markdown content.
- **Use `update_todo_list`** to show your planning progress to the user.
- **Use `request_user_input`** for structured questions during planning.
- When the user requests revision of the plan, address their feedback fully and save an updated plan.

## Plan Format

The plan should include:
- A clear title
- A brief summary
- Key changes grouped by subsystem or behavior
- Important API/interface changes
- Test cases and scenarios
- Explicit assumptions and defaults chosen

Keep the plan concise and actionable. Prefer behavior-level descriptions over file-by-file inventories. Mention specific files only when needed to disambiguate a non-obvious change."#;

pub(crate) fn agent_type_role(value: &str) -> Option<AgentRole> {
    match value.trim().to_lowercase().as_str() {
        "explorer" => Some(AgentRole::Explorer),
        "worker" | "default" | "" => Some(AgentRole::Executor),
        _ => None,
    }
}

pub(crate) fn task_role_system_prompt(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Planner => PLANNER_SYSTEM_PROMPT,
        AgentRole::Explorer => {
            "You are an Explorer subagent for a task. Investigate code, docs, and relevant context using read-only exploration unless explicitly told otherwise. Return concise findings with concrete files, commands, or sources that help the planner decide."
        }
        AgentRole::Executor => {
            "You are the Executor for an approved task plan. Implement the requested changes in your container, keep scope tight, run verification, and report changed files plus test results. If reviewer feedback arrives, fix the issues and rerun relevant checks.\n\nWhen you have produced deliverable files (reports, generated code, data exports, documents, etc.), use the `save_artifact` tool to register each file so the user can download it. Always call `save_artifact` for any final output the user would want to keep."
        }
        AgentRole::Reviewer => {
            "You are the Reviewer for a task workflow. Review executor changes for bugs, regressions, missing tests, and unclear behavior. You must call submit_review_result with passed, findings, and summary before finishing. Set passed=true only when there are no blocking issues."
        }
    }
}

pub(crate) fn initial_session_record(task_owned: bool) -> AgentSessionRecord {
    if task_owned {
        session_record_with_title("Task")
    } else {
        default_session_record()
    }
}

pub(crate) fn default_session_record() -> AgentSessionRecord {
    session_record_with_title("Chat 1")
}

pub(crate) fn next_chat_session_record(existing_session_count: usize) -> AgentSessionRecord {
    session_record_with_title(&format!("Chat {}", existing_session_count + 1))
}

pub(crate) fn session_record_with_title(title: &str) -> AgentSessionRecord {
    let now = now();
    AgentSessionRecord {
        summary: AgentSessionSummary {
            id: Uuid::new_v4(),
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            message_count: 0,
        },
        messages: Vec::new(),
        history: Vec::new(),
        last_context_tokens: None,
        last_turn_response: None,
    }
}

pub(crate) fn short_id(id: AgentId) -> String {
    id.to_string().chars().take(8).collect()
}

pub(crate) fn selected_session(
    sessions: &[AgentSessionRecord],
    session_id: Option<SessionId>,
) -> Option<&AgentSessionRecord> {
    if let Some(session_id) = session_id {
        return sessions
            .iter()
            .find(|session| session.summary.id == session_id);
    }
    sessions
        .iter()
        .max_by(|left, right| {
            left.summary
                .updated_at
                .cmp(&right.summary.updated_at)
                .then_with(|| left.summary.created_at.cmp(&right.summary.created_at))
        })
        .or_else(|| sessions.first())
}

pub(crate) fn recent_messages(
    sessions: &[AgentSessionRecord],
    limit: usize,
) -> (Option<SessionId>, Vec<AgentMessage>) {
    let Some(session) = selected_session(sessions, None) else {
        return (None, Vec::new());
    };
    let len = session.messages.len();
    let start = len.saturating_sub(limit);
    (Some(session.summary.id), session.messages[start..].to_vec())
}

pub(crate) fn last_turn_response(sessions: &[AgentSessionRecord]) -> Option<String> {
    sessions
        .iter()
        .filter_map(|session| session.last_turn_response.clone())
        .next_back()
}

pub(crate) fn is_agent_wait_complete(summary: &AgentSummary) -> bool {
    summary.current_turn.is_none()
        || matches!(
            summary.status,
            AgentStatus::Completed
                | AgentStatus::Failed
                | AgentStatus::Cancelled
                | AgentStatus::Deleted
                | AgentStatus::Idle
        )
}

pub(crate) fn final_wait_response(
    summary: &AgentSummary,
    recent_messages: &[AgentMessage],
    tracked_response: Option<String>,
) -> Option<String> {
    if !is_agent_wait_complete(summary) {
        return None;
    }
    tracked_response.or_else(|| {
        recent_messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Assistant)
            .map(|message| message.content.clone())
    })
}

pub(crate) fn last_activity_at(
    summary: &AgentSummary,
    recent_messages: &[AgentMessage],
    recent_events: &[ServiceEvent],
) -> DateTime<Utc> {
    let mut timestamp = summary.updated_at;
    if let Some(message) = recent_messages.last() {
        timestamp = timestamp.max(message.created_at);
    }
    if let Some(event) = recent_events.last() {
        timestamp = timestamp.max(event.timestamp);
    }
    timestamp
}

pub(crate) fn active_tool_snapshot(recent_events: &[ServiceEvent]) -> Option<Value> {
    let mut completed = HashSet::new();
    for event in recent_events.iter().rev() {
        match &event.kind {
            ServiceEventKind::ToolCompleted { call_id, .. } => {
                completed.insert(call_id.clone());
            }
            ServiceEventKind::ToolStarted {
                turn_id,
                call_id,
                tool_name,
                arguments_preview,
                arguments,
                ..
            } if !completed.contains(call_id) => {
                return Some(json!({
                    "turn_id": turn_id,
                    "call_id": call_id,
                    "tool_name": tool_name,
                    "arguments_preview": arguments_preview,
                    "arguments": arguments,
                    "started_at": event.timestamp,
                }));
            }
            _ => {}
        }
    }
    None
}
