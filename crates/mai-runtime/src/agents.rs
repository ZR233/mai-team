use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use mai_protocol::{
    AgentId, AgentMessage, AgentRole, AgentSessionSummary, AgentStatus, AgentSummary, MessageRole,
    ServiceEvent, ServiceEventKind, SessionId, now,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::state::AgentSessionRecord;

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

pub(crate) fn descendant_delete_order_from_summaries(
    root_id: AgentId,
    summaries: &[AgentSummary],
) -> Vec<AgentId> {
    let mut children: HashMap<AgentId, Vec<&AgentSummary>> = HashMap::new();
    for summary in summaries {
        if let Some(parent_id) = summary.parent_id {
            children.entry(parent_id).or_default().push(summary);
        }
    }
    for values in children.values_mut() {
        values.sort_by_key(|summary| summary.created_at);
    }

    let mut order = Vec::new();
    push_delete_order(root_id, &children, &mut order);
    order
}

fn push_delete_order(
    agent_id: AgentId,
    children: &HashMap<AgentId, Vec<&AgentSummary>>,
    order: &mut Vec<AgentId>,
) {
    if let Some(child_summaries) = children.get(&agent_id) {
        for child in child_summaries {
            push_delete_order(child.id, children, order);
        }
    }
    order.push(agent_id);
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
