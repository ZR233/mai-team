use crate::records::*;
use crate::*;

impl ConfigStore {
    pub async fn append_service_event(&self, event: &ServiceEvent) -> Result<()> {
        let mut db = self.db.clone();
        Query::<List<ServiceEventRecord>>::filter(
            ServiceEventRecord::fields()
                .sequence()
                .eq(u64_to_i64(event.sequence)),
        )
        .delete()
        .exec(&mut db)
        .await?;
        toasty::create!(ServiceEventRecord {
            sequence: u64_to_i64(event.sequence),
            timestamp: event.timestamp.to_rfc3339(),
            agent_id: event_agent_id(event).map(|id| id.to_string()),
            session_id: event_session_id(event).map(|id| id.to_string()),
            event_json: serde_json::to_string(event)?,
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn service_events_after(
        &self,
        sequence: u64,
        limit: usize,
    ) -> Result<Vec<ServiceEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut db = self.db.clone();
        let mut rows = Query::<List<ServiceEventRecord>>::all()
            .exec(&mut db)
            .await?;
        rows.retain(|row| i64_to_u64(row.sequence) > sequence);
        rows.sort_by_key(|row| row.sequence);
        rows.into_iter()
            .take(limit)
            .map(|row| serde_json::from_str::<ServiceEvent>(&row.event_json).map_err(Into::into))
            .collect()
    }

    pub async fn prune_service_events_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        let mut db = self.db.clone();
        let cutoff = cutoff.to_rfc3339();
        let rows = Query::<List<ServiceEventRecord>>::all()
            .exec(&mut db)
            .await?;
        let old_sequences = rows
            .into_iter()
            .filter(|row| row.timestamp < cutoff)
            .map(|row| row.sequence)
            .collect::<Vec<_>>();
        for sequence in &old_sequences {
            Query::<List<ServiceEventRecord>>::filter(
                ServiceEventRecord::fields().sequence().eq(*sequence),
            )
            .delete()
            .exec(&mut db)
            .await?;
        }
        Ok(old_sequences.len())
    }
}

fn event_agent_id(event: &ServiceEvent) -> Option<AgentId> {
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

pub(crate) fn event_session_id(event: &ServiceEvent) -> Option<SessionId> {
    match &event.kind {
        ServiceEventKind::TurnStarted { session_id, .. }
        | ServiceEventKind::TurnCompleted { session_id, .. }
        | ServiceEventKind::ToolStarted { session_id, .. }
        | ServiceEventKind::ToolCompleted { session_id, .. }
        | ServiceEventKind::AgentMessage { session_id, .. }
        | ServiceEventKind::AgentMessageDelta { session_id, .. }
        | ServiceEventKind::AgentMessageCompleted { session_id, .. }
        | ServiceEventKind::ReasoningDelta { session_id, .. }
        | ServiceEventKind::ReasoningCompleted { session_id, .. }
        | ServiceEventKind::ToolCallDelta { session_id, .. }
        | ServiceEventKind::SkillsActivated { session_id, .. }
        | ServiceEventKind::UserInputRequested { session_id, .. } => *session_id,
        ServiceEventKind::ContextCompacted { session_id, .. } => Some(*session_id),
        ServiceEventKind::Error { session_id, .. } => *session_id,
        _ => None,
    }
}
