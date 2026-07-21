use mai_protocol::SessionId;
use pl_protocol::{SessionSubscriptionRequest, SessionViewSnapshot};

use crate::{AgentRuntime, Result, RuntimeError};

/// 将 PL canonical subscription 投影为 Mai UUID wire identity 的会话订阅。
#[derive(Debug)]
pub struct MaiSessionEventSubscription {
    inner: pl_core::SessionEventSubscription,
    session_id: SessionId,
}

impl MaiSessionEventSubscription {
    fn new(inner: pl_core::SessionEventSubscription, session_id: SessionId) -> Self {
        Self { inner, session_id }
    }

    /// 接收下一帧，并在产品边界统一 session/turn ID。
    pub async fn recv(&mut self) -> Option<pl_protocol::SessionStreamFrame> {
        let mut frame = self.inner.recv().await?;
        crate::agent_host::project_session_stream_frame(&mut frame, self.session_id);
        Some(frame)
    }
}

impl AgentRuntime {
    /// 订阅一个 Mai wire session 对应的 PL canonical session stream。
    pub async fn subscribe_session_events(
        &self,
        session_id: SessionId,
        after_sequence: Option<u64>,
    ) -> Result<MaiSessionEventSubscription> {
        let framework_id = self.resolve_framework_session_id(session_id).await?;
        let subscription = self
            .framework_handle()?
            .subscribe_session(SessionSubscriptionRequest {
                session_id: framework_id,
                after_sequence,
            })
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        Ok(MaiSessionEventSubscription::new(subscription, session_id))
    }

    /// 读取包含 transient overlay 的 authoritative session projection。
    pub async fn session_event_snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<SessionViewSnapshot> {
        let framework_id = self.resolve_framework_session_id(session_id).await?;
        let framework_id = pl_core::SessionId::new(framework_id)
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        let mut snapshot = self
            .framework_handle()?
            .session_snapshot(&framework_id)
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        crate::agent_host::project_session_view_snapshot(&mut snapshot, session_id);
        Ok(snapshot)
    }

    /// 读取 review run 归档所需的最近 durable canonical 事件。
    pub(crate) async fn session_recent_events(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<pl_protocol::SessionEventEnvelope>> {
        let framework_id = self.resolve_framework_session_id(session_id).await?;
        let Some(projection) = self
            .deps
            .store
            .load_session_projection(&framework_id)
            .await?
        else {
            return Ok(Vec::new());
        };
        let start = projection.durable_events.len().saturating_sub(limit);
        projection.durable_events[start..]
            .iter()
            .map(|event| {
                let mut event = serde_json::from_value(event.payload.clone())
                    .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
                crate::agent_host::project_session_event_envelope(&mut event, session_id);
                Ok(event)
            })
            .collect()
    }

    async fn resolve_framework_session_id(&self, session_id: SessionId) -> Result<String> {
        for runtime in self.deps.store.load_agent_runtimes().await? {
            if let Some(session) = runtime
                .sessions
                .into_iter()
                .find(|session| crate::agent_host::protocol_uuid(&session.session_id) == session_id)
            {
                return Ok(session.session_id);
            }
        }
        Err(RuntimeError::SessionEventNotFound(session_id))
    }
}
