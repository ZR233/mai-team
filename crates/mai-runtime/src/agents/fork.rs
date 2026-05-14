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
    let parent_session = {
        let sessions = parent.sessions.lock().await;
        selected_session(&sessions, None).cloned()
    }
    .ok_or(RuntimeError::AgentNotFound(parent_id))?;
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
        child_session.messages = parent_session.messages.clone();
        child_session.history = parent_session.history.clone();
        child_session.summary.message_count = child_session.messages.len();
        child_session.summary.updated_at = now();
        let summary = child_session.summary.clone();
        ops.save_agent_session(child_id, &summary).await?;
    }
    ops.replace_agent_history(child_id, child_session_id, &parent_session.history)
        .await?;
    for (position, message) in parent_session.messages.iter().enumerate() {
        ops.append_agent_message(child_id, child_session_id, position, message)
            .await?;
    }
    Ok(())
}
