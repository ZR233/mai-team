use crate::records::*;
use crate::*;
use rusqlite::Connection;
use rusqlite::params;

const PRODUCT_EVENT_SQLITE_BUSY_TIMEOUT_SECS: u64 = 30;

impl MaiStore {
    pub async fn append_product_event(&self, event: &MaiProductEventEnvelope) -> Result<()> {
        let mut db = self.db.clone();
        Query::<List<MaiProductEventRecord>>::filter(
            MaiProductEventRecord::fields()
                .sequence()
                .eq(u64_to_i64(event.sequence)),
        )
        .delete()
        .exec(&mut db)
        .await?;
        toasty::create!(MaiProductEventRecord {
            sequence: u64_to_i64(event.sequence),
            timestamp: event.timestamp.to_rfc3339(),
            agent_id: event_agent_id(event).map(|id| id.to_string()),
            event_json: serde_json::to_string(event)?,
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn product_events_after(
        &self,
        sequence: u64,
        limit: usize,
    ) -> Result<Vec<MaiProductEventEnvelope>> {
        product_events_after_on_path(&self.path, sequence, limit).await
    }

    pub async fn prune_product_events_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        prune_product_events_before_on_path(&self.path, cutoff).await
    }

    pub async fn prune_product_events_to_limit(&self, limit: usize) -> Result<usize> {
        prune_product_events_to_limit_on_path(&self.path, limit).await
    }
}

pub(crate) async fn product_events_after_on_path(
    path: &Path,
    sequence: u64,
    limit: usize,
) -> Result<Vec<MaiProductEventEnvelope>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let connection = open_product_event_connection(&path)?;
        let mut statement = connection.prepare(
            "SELECT event_json FROM product_events \
             WHERE sequence > ?1 ORDER BY sequence ASC LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![u64_to_i64(sequence), usize_to_i64(limit)], |row| {
                row.get::<_, String>(0)
            })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str::<MaiProductEventEnvelope>(&row?)?);
        }
        Ok(events)
    })
    .await
    .map_err(|err| StoreError::InvalidConfig(format!("product event query task failed: {err}")))?
}

pub(crate) async fn recent_product_events_on_path(
    path: &Path,
    limit: usize,
) -> Result<Vec<MaiProductEventEnvelope>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let connection = open_product_event_connection(&path)?;
        let mut statement = connection.prepare(
            "SELECT event_json FROM (\
                 SELECT sequence, event_json FROM product_events \
                 ORDER BY sequence DESC LIMIT ?1\
             ) ORDER BY sequence ASC",
        )?;
        let rows =
            statement.query_map(params![usize_to_i64(limit)], |row| row.get::<_, String>(0))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str::<MaiProductEventEnvelope>(&row?)?);
        }
        Ok(events)
    })
    .await
    .map_err(|err| StoreError::InvalidConfig(format!("product event query task failed: {err}")))?
}

pub(crate) async fn next_product_event_sequence_on_path(path: &Path) -> Result<u64> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let connection = open_product_event_connection(&path)?;
        let max_sequence = connection.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM product_events",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(i64_to_u64(max_sequence).saturating_add(1))
    })
    .await
    .map_err(|err| StoreError::InvalidConfig(format!("product event query task failed: {err}")))?
}

async fn prune_product_events_before_on_path(path: &Path, cutoff: DateTime<Utc>) -> Result<usize> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut connection = open_product_event_connection(&path)?;
        let transaction = connection.transaction()?;
        let removed = transaction.execute(
            "DELETE FROM product_events WHERE timestamp < ?1",
            params![cutoff.to_rfc3339()],
        )?;
        transaction.commit()?;
        Ok(removed)
    })
    .await
    .map_err(|err| StoreError::InvalidConfig(format!("product event prune task failed: {err}")))?
}

async fn prune_product_events_to_limit_on_path(path: &Path, limit: usize) -> Result<usize> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut connection = open_product_event_connection(&path)?;
        let transaction = connection.transaction()?;
        let total = transaction.query_row("SELECT COUNT(*) FROM product_events", [], |row| {
            row.get::<_, i64>(0)
        })?;
        let keep = usize_to_i64(limit);
        let remove_count = total.saturating_sub(keep).max(0);
        if remove_count == 0 {
            return Ok(0);
        }
        let removed = transaction.execute(
            "DELETE FROM product_events WHERE sequence IN (\
                 SELECT sequence FROM product_events \
                 ORDER BY sequence ASC LIMIT ?1\
             )",
            params![remove_count],
        )?;
        transaction.commit()?;
        Ok(removed)
    })
    .await
    .map_err(|err| StoreError::InvalidConfig(format!("product event prune task failed: {err}")))?
}

fn open_product_event_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(
        PRODUCT_EVENT_SQLITE_BUSY_TIMEOUT_SECS,
    ))?;
    Ok(connection)
}

fn event_agent_id(event: &MaiProductEventEnvelope) -> Option<AgentId> {
    match &event.kind {
        MaiProductEventKind::AgentCreated { agent }
        | MaiProductEventKind::AgentUpdated { agent } => Some(agent.id),
        MaiProductEventKind::AgentDeleted { agent_id }
        | MaiProductEventKind::McpServerStatusChanged { agent_id, .. } => Some(*agent_id),
        MaiProductEventKind::OperationFailed { agent_id, .. } => *agent_id,
        MaiProductEventKind::TaskCreated { .. }
        | MaiProductEventKind::TaskUpdated { .. }
        | MaiProductEventKind::TaskDeleted { .. }
        | MaiProductEventKind::ProjectCreated { .. }
        | MaiProductEventKind::ProjectUpdated { .. }
        | MaiProductEventKind::ProjectDeleted { .. }
        | MaiProductEventKind::GithubWebhookReceived { .. }
        | MaiProductEventKind::ProjectReviewQueued { .. }
        | MaiProductEventKind::PlanUpdated { .. } => None,
        MaiProductEventKind::ArtifactCreated { artifact } => Some(artifact.agent_id),
    }
}
