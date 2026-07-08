use mai_protocol::{AgentId, AgentMessage, MessageRole, SessionId, now};
use mai_store::ConfigStore;
use pl_protocol::{Message, MessageContent, MessageRole as ModelMessageRole};
#[cfg(test)]
use pl_protocol::{ToolCallHistoryMetadata, ToolCallKind, ToolResultMetadata};

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
    if pl_core::repair_incomplete_tool_history(&mut history) {
        replace_session_history(store, agent, agent_id, session_id, history.clone()).await?;
    }
    Ok(history)
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
