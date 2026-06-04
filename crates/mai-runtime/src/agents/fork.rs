use mai_protocol::{AgentId, now};

use super::{AgentServiceOps, selected_session};
use crate::{Result, RuntimeError};

pub(crate) async fn fork_agent_context(
    ops: &dyn AgentServiceOps,
    parent_id: AgentId,
    child_id: AgentId,
) -> Result<()> {
    let parent = ops.agent(parent_id).await?;
    let child = ops.agent(child_id).await?;
    let (parent_session_id, parent_messages) = {
        let sessions = parent.sessions.lock().await;
        selected_session(&sessions, None)
            .map(|session| (session.summary.id, session.messages.clone()))
    }
    .ok_or(RuntimeError::AgentNotFound(parent_id))?;
    let parent_history = ops.load_agent_history(parent_id, parent_session_id).await?;
    let child_session_id = ops.resolve_session_id(child_id, None).await?;
    {
        let mut child_sessions = child.sessions.lock().await;
        let child_session = child_sessions
            .iter_mut()
            .find(|session| session.summary.id == child_session_id)
            .ok_or(RuntimeError::SessionNotFound {
                agent_id: child_id,
                session_id: child_session_id,
            })?;
        child_session.messages = parent_messages.clone();
        child_session.summary.message_count = child_session.messages.len();
        child_session.summary.updated_at = now();
        let summary = child_session.summary.clone();
        ops.save_agent_session(child_id, &summary).await?;
    }
    ops.replace_agent_history(child_id, child_session_id, &parent_history)
        .await?;
    for (position, message) in parent_messages.iter().enumerate() {
        ops.append_agent_message(child_id, child_session_id, position, message)
            .await?;
    }
    Ok(())
}
