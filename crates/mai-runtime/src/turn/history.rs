use mai_protocol::{
    AgentId, AgentMessage, MessageRole, ModelContentItem, ModelInputItem, ModelOutputItem,
    SessionId, now,
};
use mai_store::ConfigStore;
use std::collections::HashSet;

use crate::state::AgentRecord;
use crate::{Result, RuntimeError};

const COMPACT_RECENT_ASSISTANT_MAX_CHARS: usize = 8_000;
const COMPACT_RECENT_TOOL_OUTPUT_MAX_CHARS: usize = 4_000;
const COMPACT_RECENT_ASSISTANT_ITEMS: usize = 2;
const COMPACT_RECENT_TOOL_OUTPUT_ITEMS: usize = 3;

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
    {
        let sessions = agent.sessions.lock().await;
        sessions
            .iter()
            .find(|session| session.summary.id == session_id)
            .ok_or(RuntimeError::SessionNotFound {
                agent_id,
                session_id,
            })?;
    }
    let position = store.agent_history_len(agent_id, session_id).await?;
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
    {
        let sessions = agent.sessions.lock().await;
        sessions
            .iter()
            .find(|session| session.summary.id == session_id)
            .ok_or(RuntimeError::SessionNotFound {
                agent_id,
                session_id,
            })?;
    }
    let mut history = store.load_agent_history(agent_id, session_id).await?;
    if repair_incomplete_tool_history(&mut history) {
        replace_session_history(store, agent, agent_id, session_id, history.clone()).await?;
    }
    Ok(history)
}

pub(crate) async fn raw_session_history_len(
    store: &ConfigStore,
    agent: &AgentRecord,
    agent_id: AgentId,
    session_id: SessionId,
) -> Result<usize> {
    {
        let sessions = agent.sessions.lock().await;
        sessions
            .iter()
            .find(|session| session.summary.id == session_id)
            .ok_or(RuntimeError::SessionNotFound {
                agent_id,
                session_id,
            })?;
    }
    store
        .agent_history_len(agent_id, session_id)
        .await
        .map_err(Into::into)
}

pub(crate) fn compact_summary_from_output(output: &[ModelOutputItem]) -> Option<String> {
    output.iter().rev().find_map(|item| {
        let text = match item {
            ModelOutputItem::Message { text } => text,
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
    replacement.extend(recent_compaction_tail(history, summary_prefix));
    replacement.push(ModelInputItem::user_text(compact_summary_message(
        summary,
        summary_prefix,
    )));
    replacement
}

pub(crate) fn build_compaction_request_history(
    history: &[ModelInputItem],
    max_user_chars: usize,
    summary_prefix: &str,
) -> Vec<ModelInputItem> {
    let mut input = Vec::new();
    if let Some(summary) = latest_compact_summary(history, summary_prefix) {
        input.push(ModelInputItem::user_text(summary.to_string()));
    }
    input.extend(
        recent_user_messages_since_latest_summary(history, max_user_chars, summary_prefix)
            .into_iter()
            .map(ModelInputItem::user_text),
    );
    input.extend(recent_compaction_tail(history, summary_prefix));
    if input.is_empty() {
        return history.to_vec();
    }
    input
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

fn recent_user_messages_since_latest_summary(
    history: &[ModelInputItem],
    max_chars: usize,
    summary_prefix: &str,
) -> Vec<String> {
    let start = latest_compact_summary_index(history, summary_prefix)
        .map(|index| index + 1)
        .unwrap_or_default();
    recent_user_messages(&history[start..], max_chars, summary_prefix)
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

fn recent_compaction_tail(history: &[ModelInputItem], summary_prefix: &str) -> Vec<ModelInputItem> {
    let mut selected = Vec::new();
    let mut assistant_items = 0;
    let mut tool_output_items = 0;
    for item in history.iter().rev() {
        match item {
            ModelInputItem::Message { role, content } if role == "assistant" => {
                if assistant_items >= COMPACT_RECENT_ASSISTANT_ITEMS {
                    continue;
                }
                let Some(text) = assistant_output_text(content) else {
                    continue;
                };
                if text.trim().is_empty() {
                    continue;
                }
                selected.push(ModelInputItem::assistant_text(take_last_chars(
                    text,
                    COMPACT_RECENT_ASSISTANT_MAX_CHARS,
                )));
                assistant_items += 1;
            }
            ModelInputItem::FunctionCallOutput { call_id, output } => {
                if tool_output_items >= COMPACT_RECENT_TOOL_OUTPUT_ITEMS {
                    continue;
                }
                selected.push(ModelInputItem::user_text(format!(
                    "Recent tool result `{call_id}` retained for context checkpoint:\n{}",
                    compact_tool_output(output, COMPACT_RECENT_TOOL_OUTPUT_MAX_CHARS)
                )));
                tool_output_items += 1;
            }
            ModelInputItem::Message { role, content } if role == "user" => {
                let Some(text) = input_text(content) else {
                    continue;
                };
                if is_compact_summary(text.trim(), summary_prefix) {
                    break;
                }
            }
            ModelInputItem::Message { .. }
            | ModelInputItem::Reasoning { .. }
            | ModelInputItem::FunctionCall { .. } => {}
        }
    }
    selected.reverse();
    selected
}

fn latest_compact_summary<'a>(
    history: &'a [ModelInputItem],
    summary_prefix: &str,
) -> Option<&'a str> {
    latest_compact_summary_index(history, summary_prefix)
        .and_then(|index| user_message_text(&history[index]))
}

fn latest_compact_summary_index(history: &[ModelInputItem], summary_prefix: &str) -> Option<usize> {
    history.iter().enumerate().rev().find_map(|(index, item)| {
        let text = user_message_text(item)?;
        is_compact_summary(text.trim(), summary_prefix).then_some(index)
    })
}

fn compact_tool_output(output: &str, max_chars: usize) -> String {
    if output.chars().count() <= max_chars {
        return output.to_string();
    }
    let tail = take_last_chars(output, max_chars);
    format!("tool output truncated for context compaction; kept last {max_chars} chars\n{tail}")
}

fn assistant_output_text(content: &[ModelContentItem]) -> Option<&str> {
    content.iter().find_map(|item| match item {
        ModelContentItem::OutputText { text } => Some(text.as_str()),
        ModelContentItem::InputText { .. } => None,
    })
}

fn input_text(content: &[ModelContentItem]) -> Option<&str> {
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
