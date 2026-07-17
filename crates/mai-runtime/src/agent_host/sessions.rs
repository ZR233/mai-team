use chrono::{DateTime, Utc};
use mai_protocol::{AgentMessage, AgentSessionSummary, MessageRole, SessionId, TokenUsage};
use mai_store::{StoredAgentRuntime, StoredAgentRuntimeSession, StoredTokenUsage};
use pl_protocol::ModelContextItem;

use crate::{Result, RuntimeError};

/// mai 对 canonical PL session 的只读产品投影。
#[derive(Debug, Clone)]
pub(crate) struct SessionProjection {
    pub(crate) framework_id: String,
    pub(crate) summary: AgentSessionSummary,
    pub(crate) messages: Vec<AgentMessage>,
    pub(crate) last_context_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedAgentSessionId {
    pub(crate) protocol: SessionId,
    pub(crate) framework: pl_core::SessionId,
}

pub(crate) async fn load_runtime(
    store: &mai_store::MaiStore,
    runtime_agent_id: &pl_core::AgentId,
) -> Result<StoredAgentRuntime> {
    store
        .load_agent_runtime(runtime_agent_id.as_str())
        .await?
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "canonical runtime state is missing for agent `{runtime_agent_id}`"
            ))
        })
}

pub(crate) fn project_sessions(runtime: &StoredAgentRuntime) -> Vec<SessionProjection> {
    runtime.sessions.iter().map(project_session).collect()
}

pub(crate) fn selected_session(
    sessions: &[SessionProjection],
    requested: Option<SessionId>,
) -> Option<&SessionProjection> {
    if let Some(requested) = requested {
        return sessions
            .iter()
            .find(|session| session.summary.id == requested);
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

pub(crate) fn aggregate_usage(runtime: &StoredAgentRuntime) -> TokenUsage {
    let mut total = TokenUsage::default();
    for session in &runtime.sessions {
        total.add(&project_usage(&session.usage));
    }
    total
}

pub(crate) fn last_assistant_response(runtime: &StoredAgentRuntime) -> Option<String> {
    runtime
        .sessions
        .iter()
        .flat_map(|session| session.messages.iter())
        .rev()
        .find(|message| message.role == MessageRole::Assistant)
        .map(|message| message.content.clone())
}

pub(crate) fn history_messages(
    runtime: &StoredAgentRuntime,
    session_id: SessionId,
) -> Result<Option<Vec<pl_protocol::Message>>> {
    let Some(session) = runtime
        .sessions
        .iter()
        .find(|session| super::protocol_uuid(&session.session_id) == session_id)
    else {
        return Ok(None);
    };
    let items = session
        .history_items
        .iter()
        .cloned()
        .map(serde_json::from_value::<ModelContextItem>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
    Ok(Some(
        pl_core::AgentSession::from_items(items).messages().to_vec(),
    ))
}

pub(crate) fn session_state(id: pl_core::SessionId, title: String) -> pl_core::AgentSessionState {
    let timestamp = Utc::now().timestamp();
    let mut session = pl_core::AgentSessionState::empty(id);
    session.metadata = serde_json::json!({
        "title": title,
        "createdAt": timestamp,
        "updatedAt": timestamp,
    });
    session
}

fn project_session(session: &StoredAgentRuntimeSession) -> SessionProjection {
    SessionProjection {
        framework_id: session.session_id.clone(),
        summary: AgentSessionSummary {
            id: super::protocol_uuid(&session.session_id),
            title: session
                .title
                .clone()
                .unwrap_or_else(|| "New session".to_string()),
            created_at: timestamp(session.created_at),
            updated_at: timestamp(session.updated_at),
            message_count: session.messages.len(),
            token_usage: project_usage(&session.usage),
        },
        messages: session.messages.clone(),
        last_context_tokens: session.last_context_tokens,
    }
}

fn project_usage(usage: &StoredTokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.prompt_tokens,
        cached_input_tokens: usage.cached_prompt_tokens,
        output_tokens: usage.completion_tokens,
        reasoning_output_tokens: usage.reasoning_tokens,
        total_tokens: usage.total_tokens,
    }
}

fn timestamp(value: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(value, 0).unwrap_or_else(Utc::now)
}
