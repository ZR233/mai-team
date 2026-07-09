use mai_protocol::{AgentId, AgentMessage, MessageRole, now};
use pl_protocol::{Message as ModelMessage, MessageRole as ModelMessageRole};

use super::AgentServiceOps;
use crate::{Result, RuntimeError};

pub(crate) async fn fork_agent_context(
    ops: &dyn AgentServiceOps,
    child_id: AgentId,
    history: Vec<ModelMessage>,
) -> Result<()> {
    let child = ops.agent(child_id).await?;
    let child_session_id = ops.resolve_session_id(child_id, None).await?;
    let child_messages = agent_messages_from_history(&history);
    {
        let mut child_sessions = child.sessions.lock().await;
        let child_session = child_sessions
            .iter_mut()
            .find(|session| session.summary.id == child_session_id)
            .ok_or(RuntimeError::SessionNotFound {
                agent_id: child_id,
                session_id: child_session_id,
            })?;
        child_session.messages = child_messages.clone();
        child_session.summary.message_count = child_session.messages.len();
        child_session.summary.updated_at = now();
        let summary = child_session.summary.clone();
        ops.save_agent_session(child_id, &summary).await?;
    }
    ops.replace_agent_history(child_id, child_session_id, &history)
        .await?;
    for (position, message) in child_messages.iter().enumerate() {
        ops.append_agent_message(child_id, child_session_id, position, message)
            .await?;
    }
    Ok(())
}

fn agent_messages_from_history(history: &[ModelMessage]) -> Vec<AgentMessage> {
    let created_at = now();
    history
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                ModelMessageRole::User => MessageRole::User,
                ModelMessageRole::Assistant => MessageRole::Assistant,
                ModelMessageRole::System | ModelMessageRole::Tool => return None,
            };
            let content = pl_core::message_content_text_lines(&message.content);
            (!content.trim().is_empty()).then_some(AgentMessage {
                role,
                content,
                created_at,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn fork_history_text_projection_delegates_to_pl_core() {
        let source = include_str!("fork.rs");

        assert!(source.contains("pl_core::message_content_text_lines"));
        assert!(
            !source.contains(&format!("{}{}", "ContentPart", "::Text")),
            "fork UI 历史投影不应复制 pl-core multipart 文本提取逻辑"
        );
    }
}
