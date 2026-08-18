use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::*;

const CLEANUP_SQLITE_BUSY_TIMEOUT_SECS: u64 = 30;

/// Review 终态后需要幂等回收的资源类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProjectReviewCleanupResourceKind {
    ReviewerAgent,
    ReviewContext,
    ToolOutputNamespace,
}

impl fmt::Display for ProjectReviewCleanupResourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReviewerAgent => "reviewer_agent",
            Self::ReviewContext => "review_context",
            Self::ToolOutputNamespace => "tool_output_namespace",
        })
    }
}

impl FromStr for ProjectReviewCleanupResourceKind {
    type Err = StoreError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "reviewer_agent" => Ok(Self::ReviewerAgent),
            "review_context" => Ok(Self::ReviewContext),
            "tool_output_namespace" => Ok(Self::ToolOutputNamespace),
            other => Err(StoreError::InvalidConfig(format!(
                "unknown project review cleanup resource kind: {other}"
            ))),
        }
    }
}

/// 持久化清理任务的调度状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectReviewCleanupTaskStatus {
    Pending,
    Running,
    RetryWaiting,
    Succeeded,
}

impl fmt::Display for ProjectReviewCleanupTaskStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::RetryWaiting => "retry_waiting",
            Self::Succeeded => "succeeded",
        })
    }
}

impl FromStr for ProjectReviewCleanupTaskStatus {
    type Err = StoreError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "retry_waiting" => Ok(Self::RetryWaiting),
            "succeeded" => Ok(Self::Succeeded),
            other => Err(StoreError::InvalidConfig(format!(
                "unknown project review cleanup task status: {other}"
            ))),
        }
    }
}

/// 可跨重启继续执行的单项 review 资源清理任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectReviewCleanupTask {
    pub id: String,
    pub job_id: Uuid,
    pub project_id: ProjectId,
    pub resource_kind: ProjectReviewCleanupResourceKind,
    pub resource_id: String,
    pub status: ProjectReviewCleanupTaskStatus,
    pub attempt_count: u32,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl MaiStore {
    pub async fn load_live_project_review_context_run_ids(&self) -> Result<Vec<Uuid>> {
        Ok(self
            .load_live_project_review_contexts()
            .await?
            .into_iter()
            .map(|(_, run_id)| run_id)
            .collect())
    }

    pub async fn load_live_project_review_contexts(&self) -> Result<Vec<(ProjectId, Uuid)>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_cleanup_connection(&path)?;
            let mut statement = connection.prepare(
                "SELECT project_id, active_run_id FROM project_review_jobs
                 WHERE status NOT IN ('succeeded','failed','cancelled','superseded','skipped')
                   AND reviewer_agent_id IS NOT NULL
                   AND active_run_id IS NOT NULL
                 ORDER BY project_id ASC, active_run_id ASC",
            )?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .map(|value| {
                    let (project_id, run_id) = value?;
                    Ok((parse_project_id(&project_id)?, parse_uuid(&run_id)?))
                })
                .collect()
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("live review context lookup task failed: {error}"))
        })?
    }

    pub async fn claim_due_project_review_cleanup_task(
        &self,
        owner: String,
        now: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> Result<Option<ProjectReviewCleanupTask>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_cleanup_connection(&path)?;
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let task_id = transaction
                .query_row(
                    "SELECT task.id FROM project_review_cleanup_tasks task
                     JOIN project_review_jobs job ON job.id = task.job_id
                     WHERE task.status IN ('pending','retry_waiting','running')
                     AND job.active_run_id IS NULL
                     AND (task.next_attempt_at IS NULL OR task.next_attempt_at <= ?1)
                     AND (task.lease_expires_at IS NULL OR task.lease_expires_at <= ?1)
                     ORDER BY task.created_at ASC, task.id ASC LIMIT 1",
                    params![now.to_rfc3339()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let Some(task_id) = task_id else {
                transaction.commit()?;
                return Ok(None);
            };
            transaction.execute(
                "UPDATE project_review_cleanup_tasks
                 SET status = 'running', attempt_count = attempt_count + 1,
                     lease_owner = ?1, lease_expires_at = ?2, updated_at = ?3
                 WHERE id = ?4",
                params![
                    owner,
                    lease_expires_at.to_rfc3339(),
                    now.to_rfc3339(),
                    task_id
                ],
            )?;
            let task = load_cleanup_task(&transaction, &task_id)?
                .ok_or_else(|| StoreError::InvalidConfig("cleanup task vanished".to_string()))?;
            transaction.commit()?;
            Ok(Some(task))
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("cleanup task claim task failed: {error}"))
        })?
    }

    pub async fn complete_project_review_cleanup_task(
        &self,
        task_id: String,
        owner: String,
        finished_at: DateTime<Utc>,
    ) -> Result<bool> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_cleanup_connection(&path)?;
            Ok(connection.execute(
                "UPDATE project_review_cleanup_tasks
                 SET status = 'succeeded', next_attempt_at = NULL, lease_owner = NULL,
                     lease_expires_at = NULL, last_error = NULL, updated_at = ?1,
                     finished_at = ?1
                 WHERE id = ?2 AND lease_owner = ?3",
                params![finished_at.to_rfc3339(), task_id, owner],
            )? == 1)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("cleanup task completion task failed: {error}"))
        })?
    }

    pub async fn retry_project_review_cleanup_task(
        &self,
        task_id: String,
        owner: String,
        next_attempt_at: DateTime<Utc>,
        error: String,
    ) -> Result<bool> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_cleanup_connection(&path)?;
            Ok(connection.execute(
                "UPDATE project_review_cleanup_tasks
                 SET status = 'retry_waiting', next_attempt_at = ?1, lease_owner = NULL,
                     lease_expires_at = NULL, last_error = ?2, updated_at = ?3
                 WHERE id = ?4 AND lease_owner = ?5",
                params![
                    next_attempt_at.to_rfc3339(),
                    error,
                    Utc::now().to_rfc3339(),
                    task_id,
                    owner
                ],
            )? == 1)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("cleanup task retry task failed: {error}"))
        })?
    }

    pub async fn load_project_review_cleanup_tasks(
        &self,
        job_id: Uuid,
    ) -> Result<Vec<ProjectReviewCleanupTask>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_cleanup_connection(&path)?;
            let mut statement = connection.prepare(
                "SELECT id, job_id, project_id, resource_kind, resource_id, status,
                        attempt_count, next_attempt_at, lease_owner, lease_expires_at,
                        last_error, created_at, updated_at, finished_at
                 FROM project_review_cleanup_tasks WHERE job_id = ?1
                 ORDER BY resource_kind ASC, resource_id ASC",
            )?;
            let rows = statement.query_map(params![job_id.to_string()], cleanup_task_from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .map(ProjectReviewCleanupTask::try_from)
                .collect()
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("cleanup task list task failed: {error}"))
        })?
    }
}

#[derive(Debug)]
struct CleanupTaskRow {
    id: String,
    job_id: String,
    project_id: String,
    resource_kind: String,
    resource_id: String,
    status: String,
    attempt_count: i64,
    next_attempt_at: Option<String>,
    lease_owner: Option<String>,
    lease_expires_at: Option<String>,
    last_error: Option<String>,
    created_at: String,
    updated_at: String,
    finished_at: Option<String>,
}

impl TryFrom<CleanupTaskRow> for ProjectReviewCleanupTask {
    type Error = StoreError;

    fn try_from(row: CleanupTaskRow) -> Result<Self> {
        Ok(Self {
            id: row.id,
            job_id: parse_uuid(&row.job_id)?,
            project_id: parse_project_id(&row.project_id)?,
            resource_kind: row.resource_kind.parse()?,
            resource_id: row.resource_id,
            status: row.status.parse()?,
            attempt_count: u32::try_from(row.attempt_count).map_err(|_| {
                StoreError::InvalidConfig("cleanup attempt count out of range".to_string())
            })?,
            next_attempt_at: row
                .next_attempt_at
                .as_deref()
                .map(DateTime::parse_from_rfc3339)
                .transpose()?
                .map(|value| value.with_timezone(&Utc)),
            lease_owner: row.lease_owner,
            lease_expires_at: row
                .lease_expires_at
                .as_deref()
                .map(DateTime::parse_from_rfc3339)
                .transpose()?
                .map(|value| value.with_timezone(&Utc)),
            last_error: row.last_error,
            created_at: parse_datetime(&row.created_at)?,
            updated_at: parse_datetime(&row.updated_at)?,
            finished_at: row.finished_at.as_deref().map(parse_datetime).transpose()?,
        })
    }
}

pub(crate) fn ensure_project_review_cleanup_tasks(
    connection: &Connection,
    job_id: Uuid,
    created_at: DateTime<Utc>,
) -> Result<()> {
    let owner = connection
        .query_row(
            "SELECT project_id, reviewer_agent_id,
                    COALESCE(
                        active_run_id,
                        (SELECT id FROM project_review_runs
                         WHERE job_id = project_review_jobs.id
                         ORDER BY attempt_index DESC, started_at DESC LIMIT 1)
                    )
             FROM project_review_jobs WHERE id = ?1",
            params![job_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((project_id, reviewer_agent_id, run_id)) = owner else {
        return Ok(());
    };
    if let Some(reviewer_agent_id) = reviewer_agent_id {
        insert_cleanup_task(
            connection,
            job_id,
            &project_id,
            ProjectReviewCleanupResourceKind::ReviewerAgent,
            &reviewer_agent_id,
            created_at,
        )?;
        insert_cleanup_task(
            connection,
            job_id,
            &project_id,
            ProjectReviewCleanupResourceKind::ToolOutputNamespace,
            &reviewer_agent_id,
            created_at,
        )?;
    }
    if let Some(run_id) = run_id {
        insert_cleanup_task(
            connection,
            job_id,
            &project_id,
            ProjectReviewCleanupResourceKind::ReviewContext,
            &run_id,
            created_at,
        )?;
    }
    Ok(())
}

fn insert_cleanup_task(
    connection: &Connection,
    job_id: Uuid,
    project_id: &str,
    resource_kind: ProjectReviewCleanupResourceKind,
    resource_id: &str,
    created_at: DateTime<Utc>,
) -> Result<()> {
    let task_id = format!("{job_id}:{resource_kind}:{resource_id}");
    let created_at = created_at.to_rfc3339();
    connection.execute(
        "INSERT OR IGNORE INTO project_review_cleanup_tasks (
            id, job_id, project_id, resource_kind, resource_id, status,
            attempt_count, next_attempt_at, lease_owner, lease_expires_at,
            last_error, created_at, updated_at, finished_at
         ) VALUES (?1,?2,?3,?4,?5,'pending',0,?6,NULL,NULL,NULL,?6,?6,NULL)",
        params![
            task_id,
            job_id.to_string(),
            project_id,
            resource_kind.to_string(),
            resource_id,
            created_at
        ],
    )?;
    Ok(())
}

fn open_cleanup_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(CLEANUP_SQLITE_BUSY_TIMEOUT_SECS))?;
    Ok(connection)
}

fn load_cleanup_task(
    connection: &Connection,
    task_id: &str,
) -> Result<Option<ProjectReviewCleanupTask>> {
    let row = connection
        .query_row(
            "SELECT id, job_id, project_id, resource_kind, resource_id, status,
                    attempt_count, next_attempt_at, lease_owner, lease_expires_at,
                    last_error, created_at, updated_at, finished_at
             FROM project_review_cleanup_tasks WHERE id = ?1",
            params![task_id],
            cleanup_task_from_row,
        )
        .optional()?;
    row.map(ProjectReviewCleanupTask::try_from).transpose()
}

fn cleanup_task_from_row(row: &Row<'_>) -> rusqlite::Result<CleanupTaskRow> {
    Ok(CleanupTaskRow {
        id: row.get(0)?,
        job_id: row.get(1)?,
        project_id: row.get(2)?,
        resource_kind: row.get(3)?,
        resource_id: row.get(4)?,
        status: row.get(5)?,
        attempt_count: row.get(6)?,
        next_attempt_at: row.get(7)?,
        lease_owner: row.get(8)?,
        lease_expires_at: row.get(9)?,
        last_error: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        finished_at: row.get(13)?,
    })
}

fn parse_datetime(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}
