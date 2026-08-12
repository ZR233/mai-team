use std::str::FromStr;

use rusqlite::{OptionalExtension, params};

use crate::review_jobs::storage::open_review_job_connection;
use crate::*;

impl MaiStore {
    pub async fn load_refreshable_project_review_prs(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<u64>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            let mut statement = connection.prepare(
                "SELECT DISTINCT jobs.pr
                 FROM project_review_jobs jobs
                 LEFT JOIN project_pull_request_states lifecycle
                   ON lifecycle.project_id = jobs.project_id AND lifecycle.pr = jobs.pr
                 WHERE jobs.project_id = ?1
                   AND (lifecycle.project_id IS NULL OR lifecycle.state = 'closed')
                 ORDER BY jobs.pr ASC",
            )?;
            let rows =
                statement.query_map(params![project_id.to_string()], |row| row.get::<_, i64>(0))?;
            rows.map(|row| Ok(i64_to_u64(row?))).collect()
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!(
                "refreshable pull request lookup task failed: {error}"
            ))
        })?
    }

    pub async fn load_project_pull_request_state(
        &self,
        project_id: ProjectId,
        pr: u64,
    ) -> Result<Option<ProjectPullRequestLifecycleState>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_review_job_connection(&path)?;
            connection
                .query_row(
                    "SELECT state FROM project_pull_request_states
                     WHERE project_id = ?1 AND pr = ?2",
                    params![project_id.to_string(), u64_to_i64(pr)],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(StoreError::from)?
                .map(|value| {
                    ProjectPullRequestLifecycleState::from_str(&value).map_err(|error| {
                        StoreError::InvalidConfig(format!(
                            "invalid persisted pull request lifecycle state `{value}`: {error}"
                        ))
                    })
                })
                .transpose()
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("pull request state lookup task failed: {error}"))
        })?
    }

    pub async fn save_pull_request_state_observations(
        &self,
        project_id: ProjectId,
        pull_requests: Vec<PersistedPullRequestStateObservation>,
    ) -> Result<PersistedPullRequestStateSaveSummary> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_review_job_connection(&path)?;
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut summary = PersistedPullRequestStateSaveSummary::default();
            for observation in pull_requests {
                let previous = load_state_in_transaction(&transaction, project_id, observation.pr)?;
                match (previous, observation.state) {
                    (Some(ProjectPullRequestLifecycleState::Open), _) => {
                        return Err(StoreError::InvalidConfig(format!(
                            "pull request #{} persisted an open row unexpectedly",
                            observation.pr
                        )));
                    }
                    (Some(ProjectPullRequestLifecycleState::Merged), _) => {}
                    (
                        Some(ProjectPullRequestLifecycleState::Closed),
                        ProjectPullRequestLifecycleState::Open,
                    ) => {
                        transaction.execute(
                            "DELETE FROM project_pull_request_states
                             WHERE project_id = ?1 AND pr = ?2 AND state = 'closed'",
                            params![project_id.to_string(), u64_to_i64(observation.pr)],
                        )?;
                    }
                    (None, ProjectPullRequestLifecycleState::Open) => {}
                    (previous, state) => {
                        let state_changed_at = observation.state_changed_at.ok_or_else(|| {
                            StoreError::InvalidConfig(format!(
                                "pull request #{} {state} state is missing its transition time",
                                observation.pr
                            ))
                        })?;
                        transaction.execute(
                            "INSERT INTO project_pull_request_states (
                                project_id, pr, state, state_changed_at, detected_at
                             ) VALUES (?1, ?2, ?3, ?4, ?5)
                             ON CONFLICT(project_id, pr) DO UPDATE SET
                                state = excluded.state,
                                state_changed_at = excluded.state_changed_at,
                                detected_at = excluded.detected_at",
                            params![
                                project_id.to_string(),
                                u64_to_i64(observation.pr),
                                state.to_string(),
                                state_changed_at.to_rfc3339(),
                                observation.detected_at.to_rfc3339(),
                            ],
                        )?;
                        match (previous, state) {
                            (
                                Some(ProjectPullRequestLifecycleState::Closed),
                                ProjectPullRequestLifecycleState::Closed,
                            ) => {}
                            (None, ProjectPullRequestLifecycleState::Closed) => {
                                summary.newly_closed += 1
                            }
                            (
                                None | Some(ProjectPullRequestLifecycleState::Closed),
                                ProjectPullRequestLifecycleState::Merged,
                            ) => summary.newly_merged += 1,
                            (Some(ProjectPullRequestLifecycleState::Merged), _)
                            | (Some(ProjectPullRequestLifecycleState::Open), _)
                            | (_, ProjectPullRequestLifecycleState::Open) => unreachable!(),
                        }
                    }
                }
            }
            transaction.commit()?;
            Ok(summary)
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("pull request state save task failed: {error}"))
        })?
    }
}

fn load_state_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    project_id: ProjectId,
    pr: u64,
) -> Result<Option<ProjectPullRequestLifecycleState>> {
    transaction
        .query_row(
            "SELECT state FROM project_pull_request_states WHERE project_id = ?1 AND pr = ?2",
            params![project_id.to_string(), u64_to_i64(pr)],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|value| {
            ProjectPullRequestLifecycleState::from_str(&value).map_err(|error| {
                StoreError::InvalidConfig(format!(
                    "invalid persisted pull request lifecycle state `{value}`: {error}"
                ))
            })
        })
        .transpose()
}
