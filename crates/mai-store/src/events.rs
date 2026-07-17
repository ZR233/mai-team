use crate::records::*;
use crate::*;
use rusqlite::Connection;
use rusqlite::params;

const SERVICE_EVENT_SQLITE_BUSY_TIMEOUT_SECS: u64 = 30;

impl MaiStore {
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
        service_events_after_on_path(&self.path, sequence, limit).await
    }

    pub async fn prune_service_events_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        prune_service_events_before_on_path(&self.path, cutoff).await
    }

    pub async fn prune_service_events_to_limit(&self, limit: usize) -> Result<usize> {
        prune_service_events_to_limit_on_path(&self.path, limit).await
    }
}

pub(crate) async fn service_events_after_on_path(
    path: &Path,
    sequence: u64,
    limit: usize,
) -> Result<Vec<ServiceEvent>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let connection = open_service_event_connection(&path)?;
        let mut statement = connection.prepare(
            "SELECT event_json FROM service_events \
             WHERE sequence > ?1 ORDER BY sequence ASC LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![u64_to_i64(sequence), usize_to_i64(limit)], |row| {
                row.get::<_, String>(0)
            })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str::<ServiceEvent>(&row?)?);
        }
        Ok(events)
    })
    .await
    .map_err(|err| StoreError::InvalidConfig(format!("service event query task failed: {err}")))?
}

pub(crate) async fn recent_service_events_on_path(
    path: &Path,
    limit: usize,
) -> Result<Vec<ServiceEvent>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let connection = open_service_event_connection(&path)?;
        let mut statement = connection.prepare(
            "SELECT event_json FROM (\
                 SELECT sequence, event_json FROM service_events \
                 ORDER BY sequence DESC LIMIT ?1\
             ) ORDER BY sequence ASC",
        )?;
        let rows =
            statement.query_map(params![usize_to_i64(limit)], |row| row.get::<_, String>(0))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str::<ServiceEvent>(&row?)?);
        }
        Ok(events)
    })
    .await
    .map_err(|err| StoreError::InvalidConfig(format!("service event query task failed: {err}")))?
}

pub(crate) async fn next_service_event_sequence_on_path(path: &Path) -> Result<u64> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let connection = open_service_event_connection(&path)?;
        let max_sequence = connection.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM service_events",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(i64_to_u64(max_sequence).saturating_add(1))
    })
    .await
    .map_err(|err| StoreError::InvalidConfig(format!("service event query task failed: {err}")))?
}

async fn prune_service_events_before_on_path(path: &Path, cutoff: DateTime<Utc>) -> Result<usize> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut connection = open_service_event_connection(&path)?;
        let transaction = connection.transaction()?;
        let removed = transaction.execute(
            "DELETE FROM service_events WHERE timestamp < ?1",
            params![cutoff.to_rfc3339()],
        )?;
        transaction.commit()?;
        Ok(removed)
    })
    .await
    .map_err(|err| StoreError::InvalidConfig(format!("service event prune task failed: {err}")))?
}

async fn prune_service_events_to_limit_on_path(path: &Path, limit: usize) -> Result<usize> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut connection = open_service_event_connection(&path)?;
        let transaction = connection.transaction()?;
        let total = transaction.query_row("SELECT COUNT(*) FROM service_events", [], |row| {
            row.get::<_, i64>(0)
        })?;
        let keep = usize_to_i64(limit);
        let remove_count = total.saturating_sub(keep).max(0);
        if remove_count == 0 {
            return Ok(0);
        }
        let removed = transaction.execute(
            "DELETE FROM service_events WHERE sequence IN (\
                 SELECT sequence FROM service_events \
                 ORDER BY sequence ASC LIMIT ?1\
             )",
            params![remove_count],
        )?;
        transaction.commit()?;
        Ok(removed)
    })
    .await
    .map_err(|err| StoreError::InvalidConfig(format!("service event prune task failed: {err}")))?
}

fn open_service_event_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(
        SERVICE_EVENT_SQLITE_BUSY_TIMEOUT_SECS,
    ))?;
    Ok(connection)
}

fn event_agent_id(event: &ServiceEvent) -> Option<AgentId> {
    match &event.kind {
        ServiceEventKind::AgentCreated { agent } | ServiceEventKind::AgentUpdated { agent } => {
            Some(agent.id)
        }
        ServiceEventKind::AgentStateChanged { agent_id, .. }
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
