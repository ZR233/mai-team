use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mai_protocol::{AgentId, ServiceEvent, ServiceEventKind, SessionId, now};
use mai_store::ConfigStore;
use tokio::sync::{Mutex, broadcast};

pub(crate) const RECENT_EVENT_LIMIT: usize = 500;

pub(crate) struct RuntimeEvents {
    tx: broadcast::Sender<ServiceEvent>,
    sequence: AtomicU64,
    recent: Mutex<VecDeque<ServiceEvent>>,
    store: Arc<ConfigStore>,
}

impl RuntimeEvents {
    pub(crate) fn new(
        store: Arc<ConfigStore>,
        next_sequence: u64,
        recent_events: Vec<ServiceEvent>,
    ) -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            tx,
            sequence: AtomicU64::new(next_sequence),
            recent: Mutex::new(recent_events.into_iter().collect()),
            store,
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        self.tx.subscribe()
    }

    pub(crate) async fn publish(&self, kind: ServiceEventKind) {
        let event = ServiceEvent {
            sequence: self.sequence.fetch_add(1, Ordering::SeqCst),
            timestamp: now(),
            kind,
        };
        if let Err(err) = self.store.append_service_event(&event).await {
            tracing::warn!("failed to persist service event: {err}");
        }
        {
            let mut recent = self.recent.lock().await;
            if recent.len() >= RECENT_EVENT_LIMIT {
                recent.pop_front();
            }
            recent.push_back(event.clone());
        }
        let _ = self.tx.send(event);
    }

    pub(crate) async fn for_agent(&self, agent_id: AgentId) -> Vec<ServiceEvent> {
        let events = self.recent.lock().await;
        events
            .iter()
            .filter(|event| event_agent_id(event) == Some(agent_id))
            .cloned()
            .collect()
    }

    pub(crate) async fn recent_for_agent(
        &self,
        agent_id: AgentId,
        limit: usize,
    ) -> Vec<ServiceEvent> {
        let events = self.recent.lock().await;
        let mut selected = events
            .iter()
            .rev()
            .filter(|event| event_agent_id(event) == Some(agent_id))
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        selected.reverse();
        selected
    }

    pub(crate) async fn retain_since(&self, cutoff: chrono::DateTime<chrono::Utc>) {
        self.recent
            .lock()
            .await
            .retain(|event| event.timestamp >= cutoff);
    }

    pub(crate) async fn tool_metadata(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        call_id: &str,
    ) -> (Option<bool>, Option<u64>) {
        let events = self.recent.lock().await;
        events
            .iter()
            .rev()
            .find_map(|event| match &event.kind {
                ServiceEventKind::ToolCompleted {
                    agent_id: event_agent_id,
                    session_id: event_session_id,
                    call_id: event_call_id,
                    success,
                    duration_ms,
                    ..
                } if *event_agent_id == agent_id
                    && event_session_id == &Some(session_id)
                    && event_call_id == call_id =>
                {
                    Some((Some(*success), *duration_ms))
                }
                _ => None,
            })
            .unwrap_or((None, None))
    }

    #[cfg(test)]
    pub(crate) async fn snapshot(&self) -> Vec<ServiceEvent> {
        self.recent.lock().await.iter().cloned().collect()
    }
}

pub(crate) fn event_agent_id(event: &ServiceEvent) -> Option<AgentId> {
    match &event.kind {
        ServiceEventKind::AgentCreated { agent } | ServiceEventKind::AgentUpdated { agent } => {
            Some(agent.id)
        }
        ServiceEventKind::AgentStatusChanged { agent_id, .. }
        | ServiceEventKind::AgentDeleted { agent_id }
        | ServiceEventKind::TurnStarted { agent_id, .. }
        | ServiceEventKind::TurnCompleted { agent_id, .. }
        | ServiceEventKind::ToolStarted { agent_id, .. }
        | ServiceEventKind::ToolCompleted { agent_id, .. }
        | ServiceEventKind::ContextCompacted { agent_id, .. }
        | ServiceEventKind::AgentMessage { agent_id, .. }
        | ServiceEventKind::AgentMessageDelta { agent_id, .. }
        | ServiceEventKind::AgentMessageCompleted { agent_id, .. }
        | ServiceEventKind::ReasoningDelta { agent_id, .. }
        | ServiceEventKind::ReasoningCompleted { agent_id, .. }
        | ServiceEventKind::ToolCallDelta { agent_id, .. }
        | ServiceEventKind::SkillsActivated { agent_id, .. }
        | ServiceEventKind::TodoListUpdated { agent_id, .. }
        | ServiceEventKind::McpServerStatusChanged { agent_id, .. }
        | ServiceEventKind::UserInputRequested { agent_id, .. } => Some(*agent_id),
        ServiceEventKind::TaskCreated { .. }
        | ServiceEventKind::TaskUpdated { .. }
        | ServiceEventKind::TaskDeleted { .. }
        | ServiceEventKind::ProjectCreated { .. }
        | ServiceEventKind::ProjectUpdated { .. }
        | ServiceEventKind::ProjectDeleted { .. }
        | ServiceEventKind::GithubWebhookReceived { .. }
        | ServiceEventKind::ProjectReviewQueued { .. }
        | ServiceEventKind::PlanUpdated { .. } => None,
        ServiceEventKind::ArtifactCreated { artifact } => Some(artifact.agent_id),
        ServiceEventKind::Error { agent_id, .. } => *agent_id,
    }
}
