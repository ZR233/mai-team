use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::*;

const CI_WATCH_SQLITE_BUSY_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectReviewCiWatch {
    pub project_id: ProjectId,
    pub pr: u64,
    pub head_sha: String,
    pub delivery_id: Option<String>,
    pub reason: String,
    pub next_check_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl MaiStore {
    pub async fn upsert_project_review_ci_watch(&self, watch: ProjectReviewCiWatch) -> Result<()> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_ci_watch_connection(&path)?;
            upsert_project_review_ci_watch_on_connection(&connection, &watch)?;
            Ok(())
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("CI watch upsert task failed: {error}"))
        })?
    }

    pub async fn load_due_project_review_ci_watches(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ProjectReviewCiWatch>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_ci_watch_connection(&path)?;
            let mut statement = connection.prepare(
                "SELECT project_id, pr, head_sha, delivery_id, reason,
                        next_check_at, created_at, updated_at
                 FROM project_review_ci_watches
                 WHERE next_check_at <= ?1
                 ORDER BY next_check_at ASC, project_id ASC, pr ASC
                 LIMIT ?2",
            )?;
            statement
                .query_map(
                    params![now.to_rfc3339(), usize_to_i64(limit.max(1))],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, String>(7)?,
                        ))
                    },
                )?
                .map(|row| {
                    let (
                        project_id,
                        pr,
                        head_sha,
                        delivery_id,
                        reason,
                        next_check_at,
                        created_at,
                        updated_at,
                    ) = row?;
                    Ok(ProjectReviewCiWatch {
                        project_id: parse_project_id(&project_id)?,
                        pr: i64_to_u64(pr),
                        head_sha,
                        delivery_id,
                        reason,
                        next_check_at: DateTime::parse_from_rfc3339(&next_check_at)?
                            .with_timezone(&Utc),
                        created_at: DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
                        updated_at: DateTime::parse_from_rfc3339(&updated_at)?.with_timezone(&Utc),
                    })
                })
                .collect::<Result<Vec<_>>>()
        })
        .await
        .map_err(|error| StoreError::InvalidConfig(format!("CI watch load task failed: {error}")))?
    }

    pub async fn reschedule_project_review_ci_watch(
        &self,
        project_id: ProjectId,
        pr: u64,
        expected_head_sha: String,
        next_check_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<bool> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_ci_watch_connection(&path)?;
            Ok(connection.execute(
                "UPDATE project_review_ci_watches
                 SET next_check_at = ?1, updated_at = ?2
                 WHERE id = ?3 AND head_sha = ?4",
                params![
                    next_check_at.to_rfc3339(),
                    updated_at.to_rfc3339(),
                    ci_watch_id(project_id, pr),
                    expected_head_sha,
                ],
            )? == 1)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("CI watch reschedule task failed: {error}"))
        })?
    }

    pub async fn replace_project_review_ci_watch_head(
        &self,
        project_id: ProjectId,
        pr: u64,
        expected_head_sha: String,
        head_sha: String,
        next_check_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<bool> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_ci_watch_connection(&path)?;
            Ok(connection.execute(
                "UPDATE project_review_ci_watches
                 SET head_sha = ?1, next_check_at = ?2, updated_at = ?3
                 WHERE id = ?4 AND head_sha = ?5",
                params![
                    head_sha,
                    next_check_at.to_rfc3339(),
                    updated_at.to_rfc3339(),
                    ci_watch_id(project_id, pr),
                    expected_head_sha,
                ],
            )? == 1)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("CI watch head replacement task failed: {error}"))
        })?
    }

    pub async fn delete_project_review_ci_watch(
        &self,
        project_id: ProjectId,
        pr: u64,
        expected_head_sha: String,
    ) -> Result<bool> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_ci_watch_connection(&path)?;
            Ok(connection.execute(
                "DELETE FROM project_review_ci_watches
                 WHERE id = ?1 AND head_sha = ?2",
                params![ci_watch_id(project_id, pr), expected_head_sha],
            )? == 1)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("CI watch delete task failed: {error}"))
        })?
    }

    pub async fn load_project_review_ci_watch(
        &self,
        project_id: ProjectId,
        pr: u64,
    ) -> Result<Option<ProjectReviewCiWatch>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_ci_watch_connection(&path)?;
            connection
                .query_row(
                    "SELECT project_id, pr, head_sha, delivery_id, reason,
                            next_check_at, created_at, updated_at
                     FROM project_review_ci_watches WHERE id = ?1",
                    params![ci_watch_id(project_id, pr)],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, String>(7)?,
                        ))
                    },
                )
                .optional()?
                .map(ci_watch_from_row)
                .transpose()
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("CI watch lookup task failed: {error}"))
        })?
    }
}

pub(crate) fn upsert_project_review_ci_watch_on_connection(
    connection: &Connection,
    watch: &ProjectReviewCiWatch,
) -> Result<()> {
    let id = ci_watch_id(watch.project_id, watch.pr);
    connection.execute(
        "INSERT INTO project_review_ci_watches (
            id, project_id, pr, head_sha, delivery_id, reason,
            next_check_at, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
            delivery_id = CASE
                WHEN project_review_ci_watches.head_sha != excluded.head_sha
                    THEN excluded.delivery_id
                ELSE COALESCE(
                    project_review_ci_watches.delivery_id,
                    excluded.delivery_id
                )
            END,
            reason = CASE
                WHEN project_review_ci_watches.head_sha != excluded.head_sha
                    THEN excluded.reason
                ELSE project_review_ci_watches.reason
            END,
            head_sha = excluded.head_sha,
            next_check_at = excluded.next_check_at,
            updated_at = excluded.updated_at",
        params![
            id,
            watch.project_id.to_string(),
            u64_to_i64(watch.pr),
            watch.head_sha,
            watch.delivery_id,
            watch.reason,
            watch.next_check_at.to_rfc3339(),
            watch.created_at.to_rfc3339(),
            watch.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn ci_watch_from_row(
    row: (
        String,
        i64,
        String,
        Option<String>,
        String,
        String,
        String,
        String,
    ),
) -> Result<ProjectReviewCiWatch> {
    Ok(ProjectReviewCiWatch {
        project_id: parse_project_id(&row.0)?,
        pr: i64_to_u64(row.1),
        head_sha: row.2,
        delivery_id: row.3,
        reason: row.4,
        next_check_at: DateTime::parse_from_rfc3339(&row.5)?.with_timezone(&Utc),
        created_at: DateTime::parse_from_rfc3339(&row.6)?.with_timezone(&Utc),
        updated_at: DateTime::parse_from_rfc3339(&row.7)?.with_timezone(&Utc),
    })
}

fn ci_watch_id(project_id: ProjectId, pr: u64) -> String {
    format!("{project_id}:{pr}")
}

fn open_ci_watch_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(CI_WATCH_SQLITE_BUSY_TIMEOUT_SECS))?;
    Ok(connection)
}
