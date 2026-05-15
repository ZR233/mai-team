use mai_protocol::{
    AgentId, AgentMessage, MessageRole, ModelContentItem, ModelInputItem, ModelOutputItem,
    SessionId, now,
};
use mai_store::ConfigStore;
use std::collections::HashSet;

use crate::state::AgentRecord;
use crate::{Result, RuntimeError};

pub(crate) async fn record_message(
    store: &ConfigStore,
    agent: &AgentRecord,
    agent_id: AgentId,
    session_id: SessionId,
    role: MessageRole,
    content: String,
) -> Result<()> {
    let message = AgentMessage {
        role,
        content,
        created_at: now(),
    };
    let (position, session_summary) = {
        let mut sessions = agent.sessions.lock().await;
        let session = sessions
            .iter_mut()
            .find(|session| session.summary.id == session_id)
            .ok_or(RuntimeError::SessionNotFound {
                agent_id,
                session_id,
            })?;
        let position = session.messages.len();
        session.messages.push(message.clone());
        session.summary.message_count = session.messages.len();
        session.summary.updated_at = message.created_at;
        (position, session.summary.clone())
    };
    store.save_agent_session(agent_id, &session_summary).await?;
    store
        .append_agent_message(agent_id, session_id, position, &message)
        .await?;
    Ok(())
}

pub(crate) async fn record_history_item(
    store: &ConfigStore,
    agent: &AgentRecord,
    agent_id: AgentId,
    session_id: SessionId,
    item: ModelInputItem,
) -> Result<()> {
    let position = {
        let mut sessions = agent.sessions.lock().await;
        let session = sessions
            .iter_mut()
            .find(|session| session.summary.id == session_id)
            .ok_or(RuntimeError::SessionNotFound {
                agent_id,
                session_id,
            })?;
        let position = session.history.len();
        session.history.push(item.clone());
        position
    };
    store
        .append_agent_history_item(agent_id, session_id, position, &item)
        .await?;
    Ok(())
}

pub(crate) async fn replace_session_history(
    store: &ConfigStore,
    agent: &AgentRecord,
    agent_id: AgentId,
    session_id: SessionId,
    history: Vec<ModelInputItem>,
) -> Result<()> {
    store
        .replace_agent_history(agent_id, session_id, &history)
        .await?;
    {
        let mut sessions = agent.sessions.lock().await;
        let session = sessions
            .iter_mut()
            .find(|session| session.summary.id == session_id)
            .ok_or(RuntimeError::SessionNotFound {
                agent_id,
                session_id,
            })?;
        session.history = history;
        session.last_context_tokens = None;
    }
    Ok(())
}

pub(crate) async fn record_session_context_tokens(
    store: &ConfigStore,
    agent: &AgentRecord,
    agent_id: AgentId,
    session_id: SessionId,
    tokens: u64,
) -> Result<()> {
    {
        let mut sessions = agent.sessions.lock().await;
        let session = sessions
            .iter_mut()
            .find(|session| session.summary.id == session_id)
            .ok_or(RuntimeError::SessionNotFound {
                agent_id,
                session_id,
            })?;
        session.last_context_tokens = Some(tokens);
    }
    store
        .save_session_context_tokens(agent_id, session_id, tokens)
        .await?;
    Ok(())
}

pub(crate) async fn session_context_tokens(
    agent: &AgentRecord,
    agent_id: AgentId,
    session_id: SessionId,
) -> Result<Option<u64>> {
    let sessions = agent.sessions.lock().await;
    sessions
        .iter()
        .find(|session| session.summary.id == session_id)
        .map(|session| session.last_context_tokens)
        .ok_or(RuntimeError::SessionNotFound {
            agent_id,
            session_id,
        })
}

pub(crate) async fn session_history(
    store: &ConfigStore,
    agent: &AgentRecord,
    agent_id: AgentId,
    session_id: SessionId,
) -> Result<Vec<ModelInputItem>> {
    let mut history = {
        let sessions = agent.sessions.lock().await;
        sessions
            .iter()
            .find(|session| session.summary.id == session_id)
            .map(|session| session.history.clone())
            .ok_or(RuntimeError::SessionNotFound {
                agent_id,
                session_id,
            })?
    };
    if repair_incomplete_tool_history(&mut history) {
        replace_session_history(store, agent, agent_id, session_id, history.clone()).await?;
    }
    Ok(history)
}

pub(crate) async fn raw_session_history_len(
    agent: &AgentRecord,
    agent_id: AgentId,
    session_id: SessionId,
) -> Result<usize> {
    let sessions = agent.sessions.lock().await;
    sessions
        .iter()
        .find(|session| session.summary.id == session_id)
        .map(|session| session.history.len())
        .ok_or(RuntimeError::SessionNotFound {
            agent_id,
            session_id,
        })
}

pub(crate) fn compact_summary_from_output(output: &[ModelOutputItem]) -> Option<String> {
    output.iter().rev().find_map(|item| {
        let text = match item {
            ModelOutputItem::Message { text } => text,
            ModelOutputItem::Reasoning { content } => content,
            _ => return None,
        };
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_string())
    })
}

pub(crate) fn repair_incomplete_tool_history(history: &mut Vec<ModelInputItem>) -> bool {
    let mut insertions: Vec<(usize, Vec<ModelInputItem>)> = Vec::new();
    let mut i = 0;
    while i < history.len() {
        let mut call_ids = Vec::new();
        while i < history.len() {
            match &history[i] {
                ModelInputItem::FunctionCall { call_id, .. } => {
                    call_ids.push(call_id.clone());
                    i += 1;
                }
                _ => break,
            }
        }
        if call_ids.is_empty() {
            i += 1;
            continue;
        }

        let mut answered = HashSet::new();
        while i < history.len() {
            match &history[i] {
                ModelInputItem::FunctionCallOutput { call_id, .. }
                    if call_ids.iter().any(|id| id == call_id) =>
                {
                    answered.insert(call_id.clone());
                    i += 1;
                }
                _ => break,
            }
        }

        let missing_outputs = call_ids
            .into_iter()
            .filter(|call_id| !answered.contains(call_id))
            .map(|call_id| ModelInputItem::FunctionCallOutput {
                call_id,
                output: "error: tool execution interrupted".to_string(),
            })
            .collect::<Vec<_>>();
        if !missing_outputs.is_empty() {
            insertions.push((i, missing_outputs));
        }
    }
    let changed = !insertions.is_empty();
    for (pos, items) in insertions.into_iter().rev() {
        for item in items.into_iter().rev() {
            history.insert(pos, item);
        }
    }
    changed
}

pub(crate) fn build_compacted_history(
    history: &[ModelInputItem],
    summary: &str,
    max_user_chars: usize,
    summary_prefix: &str,
) -> Vec<ModelInputItem> {
    let mut replacement = recent_user_messages(history, max_user_chars, summary_prefix)
        .into_iter()
        .map(ModelInputItem::user_text)
        .collect::<Vec<_>>();
    replacement.push(ModelInputItem::user_text(compact_summary_message(
        summary,
        summary_prefix,
    )));
    replacement
}

pub(crate) fn compact_summary_message(summary: &str, summary_prefix: &str) -> String {
    format!("{}\n{}", summary_prefix, summary.trim())
}

pub(crate) fn is_compact_summary(text: &str, summary_prefix: &str) -> bool {
    text.starts_with(summary_prefix)
}

pub(crate) fn recent_user_messages(
    history: &[ModelInputItem],
    max_chars: usize,
    summary_prefix: &str,
) -> Vec<String> {
    let mut selected = Vec::new();
    let mut remaining = max_chars;
    for item in history.iter().rev() {
        if remaining == 0 {
            break;
        }
        let Some(text) = user_message_text(item) else {
            continue;
        };
        if is_compact_summary(text.trim(), summary_prefix) {
            continue;
        }
        if text.chars().count() <= remaining {
            selected.push(text.to_string());
            remaining = remaining.saturating_sub(text.chars().count());
        } else {
            selected.push(take_last_chars(text, remaining));
            break;
        }
    }
    selected.reverse();
    selected
}

pub(crate) fn user_message_text(item: &ModelInputItem) -> Option<&str> {
    let ModelInputItem::Message { role, content } = item else {
        return None;
    };
    if role != "user" {
        return None;
    }
    content.iter().find_map(|item| match item {
        ModelContentItem::InputText { text } => Some(text.as_str()),
        ModelContentItem::OutputText { .. } => None,
    })
}

fn take_last_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut chars = text.chars().rev().take(max_chars).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}
