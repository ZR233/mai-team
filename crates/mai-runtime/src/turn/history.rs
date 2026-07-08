use mai_protocol::{AgentId, AgentMessage, MessageRole, ModelOutputItem, SessionId, now};
use mai_store::ConfigStore;
use pl_protocol::{Message, MessageContent, MessageRole as ModelMessageRole, ToolResultMetadata};
use pl_protocol::{ToolCallHistoryMetadata, ToolCallKind};

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

#[cfg(test)]
pub(crate) async fn record_history_item(
    store: &ConfigStore,
    agent: &AgentRecord,
    agent_id: AgentId,
    session_id: SessionId,
    item: Message,
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
    history: Vec<Message>,
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
) -> Result<Vec<Message>> {
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

pub(crate) fn repair_incomplete_tool_history(history: &mut Vec<Message>) -> bool {
    let original_len = history.len();
    let mut repaired = Vec::with_capacity(history.len());
    let mut pending = Vec::new();

    for message in std::mem::take(history) {
        let tool_calls = tool_calls_from_message(&message);
        let tool_result = tool_result_from_message(&message);
        if !pending.is_empty() && tool_calls.is_empty() && tool_result.is_none() {
            append_missing_tool_results(&mut repaired, &mut pending);
        }
        if !tool_calls.is_empty() {
            pending.extend(tool_calls);
        }
        if let Some(tool_result) = tool_result {
            pending.retain(|call: &PendingToolCall| call.id != tool_result.tool_call_id);
        }
        repaired.push(message);
    }
    append_missing_tool_results(&mut repaired, &mut pending);
    let changed = repaired.len() != original_len;
    *history = repaired;
    changed
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingToolCall {
    id: String,
    call_id: Option<String>,
    name: String,
    kind: ToolCallKind,
    arguments: String,
}

fn append_missing_tool_results(output: &mut Vec<Message>, pending: &mut Vec<PendingToolCall>) {
    output.extend(pending.drain(..).map(missing_tool_result_message));
}

fn tool_calls_from_message(message: &Message) -> Vec<PendingToolCall> {
    let Some(metadata) = ToolCallHistoryMetadata::from_metadata(&message.metadata) else {
        return Vec::new();
    };
    let Ok(serde_json::Value::Array(calls)) =
        serde_json::from_str::<serde_json::Value>(&metadata.tool_calls_json)
    else {
        return Vec::new();
    };
    calls
        .into_iter()
        .filter_map(|call| {
            let id = call
                .get("id")
                .or_else(|| call.get("call_id"))
                .and_then(serde_json::Value::as_str)?
                .to_string();
            let call_id = call
                .get("call_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let name = call
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let payload = call.get("payload");
            let kind = payload
                .and_then(|payload| payload.get("kind"))
                .and_then(serde_json::Value::as_str)
                .map(tool_call_kind_from_str)
                .unwrap_or(ToolCallKind::Function);
            let arguments = payload
                .and_then(|payload| payload.get("arguments"))
                .or_else(|| call.get("arguments"))
                .map(tool_call_arguments)
                .unwrap_or_else(|| "{}".to_string());
            Some(PendingToolCall {
                id,
                call_id,
                name,
                kind,
                arguments,
            })
        })
        .collect()
}

fn tool_result_from_message(message: &Message) -> Option<ToolResultMetadata> {
    ToolResultMetadata::from_metadata(&message.metadata).ok()
}

fn missing_tool_result_message(call: PendingToolCall) -> Message {
    let mut metadata = Default::default();
    ToolResultMetadata::new(call.id, call.call_id, call.name, call.kind, call.arguments)
        .insert_into(&mut metadata);
    Message {
        role: ModelMessageRole::Tool,
        content: MessageContent::Text(
            "Tool call did not produce a result before the turn ended.".to_string(),
        ),
        reasoning_content: None,
        metadata,
    }
}

fn tool_call_kind_from_str(value: &str) -> ToolCallKind {
    match value {
        "custom" => ToolCallKind::Custom,
        "function" => ToolCallKind::Function,
        _ => ToolCallKind::Function,
    }
}

fn tool_call_arguments(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
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

pub(crate) fn build_compacted_history(
    history: &[Message],
    summary: &str,
    max_user_chars: usize,
    summary_prefix: &str,
) -> Vec<Message> {
    let mut replacement = recent_user_messages(history, max_user_chars, summary_prefix)
        .into_iter()
        .map(user_text_message)
        .collect::<Vec<_>>();
    replacement.extend(recent_compaction_tail(history, summary_prefix));
    replacement.push(user_text_message(compact_summary_message(
        summary,
        summary_prefix,
    )));
    replacement
}

pub(crate) fn build_compaction_request_history(
    history: &[Message],
    max_user_chars: usize,
    summary_prefix: &str,
) -> Vec<Message> {
    let mut input = Vec::new();
    if let Some(summary) = latest_compact_summary(history, summary_prefix) {
        input.push(user_text_message(summary.to_string()));
    }
    input.extend(
        recent_user_messages_since_latest_summary(history, max_user_chars, summary_prefix)
            .into_iter()
            .map(user_text_message),
    );
    input.extend(recent_compaction_tail(history, summary_prefix));
    if input.is_empty() {
        return history.to_vec();
    }
    input
}

pub(crate) fn user_text_message(text: impl Into<String>) -> Message {
    Message {
        role: ModelMessageRole::User,
        content: MessageContent::Text(text.into()),
        reasoning_content: None,
        metadata: Default::default(),
    }
}

pub(crate) fn assistant_text_message(text: impl Into<String>) -> Message {
    Message {
        role: ModelMessageRole::Assistant,
        content: MessageContent::Text(text.into()),
        reasoning_content: None,
        metadata: Default::default(),
    }
}

#[cfg(test)]
pub(crate) fn reasoning_message(content: impl Into<String>) -> Message {
    Message {
        role: ModelMessageRole::Assistant,
        content: MessageContent::Text(String::new()),
        reasoning_content: Some(content.into()),
        metadata: Default::default(),
    }
}

#[cfg(test)]
pub(crate) fn tool_call_message(call_id: String, name: String, raw_arguments: String) -> Message {
    let arguments =
        serde_json::from_str(&raw_arguments).unwrap_or(serde_json::Value::String(raw_arguments));
    let tool_calls = serde_json::json!([{
        "id": call_id,
        "name": name,
        "payload": {
            "kind": "function",
            "arguments": arguments
        },
        "call_id": call_id
    }])
    .to_string();
    let mut metadata = Default::default();
    ToolCallHistoryMetadata::new(tool_calls).insert_into(&mut metadata);
    Message {
        role: ModelMessageRole::Assistant,
        content: MessageContent::Text(String::new()),
        reasoning_content: None,
        metadata,
    }
}

#[cfg(test)]
pub(crate) fn tool_result_message(
    call_id: String,
    name: String,
    raw_arguments: String,
    output: String,
) -> Message {
    let mut metadata = Default::default();
    ToolResultMetadata::new(call_id, None, name, ToolCallKind::Function, raw_arguments)
        .insert_into(&mut metadata);
    Message {
        role: ModelMessageRole::Tool,
        content: MessageContent::Text(output),
        reasoning_content: None,
        metadata,
    }
}

pub(crate) fn compact_summary_message(summary: &str, summary_prefix: &str) -> String {
    format!("{}\n{}", summary_prefix, summary.trim())
}

pub(crate) fn is_compact_summary(text: &str, summary_prefix: &str) -> bool {
    text.starts_with(summary_prefix)
}

pub(crate) fn recent_user_messages(
    history: &[Message],
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
    history: &[Message],
    max_chars: usize,
    summary_prefix: &str,
) -> Vec<String> {
    let start = latest_compact_summary_index(history, summary_prefix)
        .map(|index| index + 1)
        .unwrap_or_default();
    recent_user_messages(&history[start..], max_chars, summary_prefix)
}

pub(crate) fn user_message_text(item: &Message) -> Option<&str> {
    if item.role != ModelMessageRole::User {
        return None;
    }
    match &item.content {
        MessageContent::Text(text) => Some(text.as_str()),
        MessageContent::MultiPart(parts) => parts.iter().find_map(|part| match part {
            pl_protocol::ContentPart::Text { text } => Some(text.as_str()),
            pl_protocol::ContentPart::Image { .. } => None,
        }),
    }
}

fn message_text(item: &Message) -> String {
    match &item.content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::MultiPart(parts) => parts
            .iter()
            .filter_map(|part| match part {
                pl_protocol::ContentPart::Text { text } => Some(text.as_str()),
                pl_protocol::ContentPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn recent_compaction_tail(history: &[Message], summary_prefix: &str) -> Vec<Message> {
    let mut selected = Vec::new();
    let mut assistant_items = 0;
    let mut tool_output_items = 0;
    for item in history.iter().rev() {
        match item.role {
            ModelMessageRole::Assistant => {
                if assistant_items >= COMPACT_RECENT_ASSISTANT_ITEMS {
                    continue;
                }
                let text = message_text(item);
                if text.trim().is_empty() {
                    continue;
                }
                selected.push(assistant_text_message(take_last_chars(
                    &text,
                    COMPACT_RECENT_ASSISTANT_MAX_CHARS,
                )));
                assistant_items += 1;
            }
            ModelMessageRole::Tool => {
                if tool_output_items >= COMPACT_RECENT_TOOL_OUTPUT_ITEMS {
                    continue;
                }
                let call_id = ToolResultMetadata::from_metadata(&item.metadata)
                    .map(|metadata| metadata.tool_call_id)
                    .unwrap_or_else(|_| "unknown".to_string());
                selected.push(user_text_message(format!(
                    "Recent tool result `{call_id}` retained for context checkpoint:\n{}",
                    compact_tool_output(&message_text(item), COMPACT_RECENT_TOOL_OUTPUT_MAX_CHARS)
                )));
                tool_output_items += 1;
            }
            ModelMessageRole::User => {
                let Some(text) = user_message_text(item) else {
                    continue;
                };
                if is_compact_summary(text.trim(), summary_prefix) {
                    break;
                }
            }
            ModelMessageRole::System => {}
        }
    }
    selected.reverse();
    selected
}

fn latest_compact_summary<'a>(history: &'a [Message], summary_prefix: &str) -> Option<&'a str> {
    latest_compact_summary_index(history, summary_prefix)
        .and_then(|index| user_message_text(&history[index]))
}

fn latest_compact_summary_index(history: &[Message], summary_prefix: &str) -> Option<usize> {
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

fn take_last_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut chars = text.chars().rev().take(max_chars).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}
