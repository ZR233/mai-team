use mai_protocol::{AgentId, AgentMessage, MessageRole, SessionId, now};
use mai_store::ConfigStore;
use pl_protocol::Message;

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
    pl_core::user_text_message(text)
}

#[cfg(test)]
pub(crate) fn assistant_text_message(text: impl Into<String>) -> Message {
    pl_core::assistant_text_message(text)
}

#[cfg(test)]
pub(crate) fn reasoning_message(content: impl Into<String>) -> Message {
    pl_core::assistant_reasoning_message(content)
}

#[cfg(test)]
pub(crate) fn tool_call_message(call_id: String, name: String, raw_arguments: String) -> Message {
    pl_core::tool_call_history_message(call_id, name, raw_arguments)
}

#[cfg(test)]
pub(crate) fn tool_result_message(
    call_id: String,
    name: String,
    raw_arguments: String,
    output: String,
) -> Message {
    pl_core::tool_result_history_message(call_id, name, raw_arguments, output)
}

#[cfg(test)]
pub(crate) fn is_compact_summary(text: &str, summary_prefix: &str) -> bool {
    pl_core::is_compaction_summary_text(text, summary_prefix)
}

#[cfg(test)]
pub(crate) fn user_message_text(item: &Message) -> Option<&str> {
    pl_core::user_message_text(item)
}

#[cfg(test)]
mod tests {
    #[test]
    fn message_helpers_delegate_to_pl_core() {
        let source = include_str!("history.rs");

        assert!(source.contains("pl_core::user_text_message"));
        assert!(source.contains("pl_core::assistant_text_message"));
        assert!(source.contains("pl_core::assistant_reasoning_message"));
        assert!(source.contains("pl_core::user_message_text"));
        assert!(source.contains("pl_core::is_compaction_summary_text"));
        assert!(
            !source.contains(&format!("{}{}", "MessageContent::Text", "(text.into())")),
            "history adapter 不应再手写模型消息构造"
        );
    }
}
