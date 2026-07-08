use mai_protocol::{AgentId, AgentMessage, MessageRole, SessionId, now};
use mai_store::ConfigStore;
use pl_protocol::{Message, MessageContent, MessageRole as ModelMessageRole, ToolResultMetadata};
use pl_protocol::{ToolCallHistoryMetadata, ToolCallKind};

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

pub(crate) fn user_text_message(text: impl Into<String>) -> Message {
    Message {
        role: ModelMessageRole::User,
        content: MessageContent::Text(text.into()),
        reasoning_content: None,
        metadata: Default::default(),
    }
}

#[cfg(test)]
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

pub(crate) fn is_compact_summary(text: &str, summary_prefix: &str) -> bool {
    text.starts_with(summary_prefix)
}

#[cfg(test)]
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
