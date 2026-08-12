use rusqlite::{OptionalExtension, params};

use crate::review_jobs::storage::open_review_job_connection;
use crate::*;

impl MaiStore {
    pub async fn load_unmerged_project_review_prs(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<u64>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            let mut statement = connection.prepare(
                "SELECT DISTINCT jobs.pr
                 FROM project_review_jobs jobs
                 LEFT JOIN project_merged_pull_requests merged
                   ON merged.project_id = jobs.project_id AND merged.pr = jobs.pr
                 WHERE jobs.project_id = ?1 AND merged.project_id IS NULL
                 ORDER BY jobs.pr ASC",
            )?;
            let rows =
                statement.query_map(params![project_id.to_string()], |row| row.get::<_, i64>(0))?;
            rows.map(|row| Ok(i64_to_u64(row?))).collect()
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("unmerged pull request lookup task failed: {error}"))
        })?
    }

    pub async fn is_project_pull_request_merged(
        &self,
        project_id: ProjectId,
        pr: u64,
    ) -> Result<bool> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            connection
                .query_row(
                    "SELECT 1 FROM project_merged_pull_requests
                     WHERE project_id = ?1 AND pr = ?2",
                    params![project_id.to_string(), u64_to_i64(pr)],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map(|value| value.is_some())
                .map_err(Into::into)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("merged pull request lookup task failed: {error}"))
        })?
    }

    pub async fn save_merged_project_pull_requests(
        &self,
        project_id: ProjectId,
        pull_requests: Vec<PersistedMergedPullRequest>,
    ) -> Result<usize> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_review_job_connection(&path)?;
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut inserted = 0;
            for pull_request in pull_requests {
                inserted += transaction.execute(
                    "INSERT INTO project_merged_pull_requests (
                        project_id, pr, merged_at, detected_at
                     ) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(project_id, pr) DO NOTHING",
                    params![
                        project_id.to_string(),
                        u64_to_i64(pull_request.pr),
                        pull_request.merged_at.to_rfc3339(),
                        pull_request.detected_at.to_rfc3339(),
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(inserted)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("merged pull request save task failed: {error}"))
        })?
    }
}
