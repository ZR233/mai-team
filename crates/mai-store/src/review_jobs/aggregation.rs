use rusqlite::params;

use super::storage::{
    PROJECT_REVIEW_JOB_COLUMNS, open_review_job_connection, project_review_job_record,
};
use super::*;

const MAX_REVIEW_PAGE_SIZE: usize = 100;

impl MaiStore {
    pub async fn load_project_pull_request_reviews(
        &self,
        project_id: ProjectId,
        page: usize,
        page_size: usize,
    ) -> Result<ProjectPullRequestReviewPage> {
        let offset = review_page_offset(page, page_size)?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_review_job_connection(&path)?;
            let transaction = connection.transaction()?;
            let (total_items, summary) = load_review_summary(&transaction, project_id)?;
            let reviews =
                load_review_page(&transaction, project_id, usize_to_i64(page_size), offset)?;
            transaction.commit()?;
            Ok(ProjectPullRequestReviewPage {
                reviews,
                page,
                page_size,
                total_items,
                total_pages: total_pages(total_items, page_size),
                summary,
            })
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("pull request review page task failed: {error}"))
        })?
    }

    pub async fn load_project_pull_request_review_history(
        &self,
        project_id: ProjectId,
        pr: u64,
        page: usize,
        page_size: usize,
    ) -> Result<ProjectPullRequestReviewHistoryPage> {
        let offset = review_page_offset(page, page_size)?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_review_job_connection(&path)?;
            let transaction = connection.transaction()?;
            let total_items = load_history_count(&transaction, project_id, pr)?;
            let items = load_history_page(
                &transaction,
                project_id,
                pr,
                usize_to_i64(page_size),
                offset,
            )?;
            transaction.commit()?;
            Ok(ProjectPullRequestReviewHistoryPage {
                items,
                page,
                page_size,
                total_items,
                total_pages: total_pages(total_items, page_size),
            })
        })
        .await
        .map_err(|error| {
            StoreError::InvalidConfig(format!("pull request review history task failed: {error}"))
        })?
    }
}

fn load_review_summary(
    transaction: &rusqlite::Transaction<'_>,
    project_id: ProjectId,
) -> Result<(usize, ProjectPullRequestReviewStatusSummary)> {
    let values = transaction.query_row(
        "WITH ranked AS (
             SELECT status,
                    ROW_NUMBER() OVER (
                        PARTITION BY pr ORDER BY created_at DESC, id DESC
                    ) AS review_rank
             FROM project_review_jobs
             WHERE project_id = ?1
         )
         SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN status IN (
                    'queued','preparing','running','retry_waiting',
                    'submission_pending','reconciling'
                ) THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'succeeded' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'skipped' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0)
         FROM ranked WHERE review_rank = 1",
        params![project_id.to_string()],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        },
    )?;
    Ok((
        count_to_usize(values.0, "review total")?,
        ProjectPullRequestReviewStatusSummary {
            active: count_to_usize(values.1, "active review total")?,
            succeeded: count_to_usize(values.2, "succeeded review total")?,
            skipped: count_to_usize(values.3, "skipped review total")?,
            failed: count_to_usize(values.4, "failed review total")?,
        },
    ))
}

fn load_review_page(
    transaction: &rusqlite::Transaction<'_>,
    project_id: ProjectId,
    page_size: i64,
    offset: i64,
) -> Result<Vec<ProjectPullRequestReviewSummary>> {
    let sql = format!(
        "WITH ranked AS (
             SELECT {PROJECT_REVIEW_JOB_COLUMNS},
                    COUNT(*) OVER (PARTITION BY pr) AS history_count,
                    ROW_NUMBER() OVER (
                        PARTITION BY pr ORDER BY created_at DESC, id DESC
                    ) AS review_rank
             FROM project_review_jobs
             WHERE project_id = ?1
         )
         SELECT {PROJECT_REVIEW_JOB_COLUMNS}, history_count
         FROM ranked WHERE review_rank = 1
         ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3"
    );
    let mut statement = transaction.prepare(&sql)?;
    let rows = statement.query_map(params![project_id.to_string(), page_size, offset], |row| {
        Ok((project_review_job_record(row)?, row.get::<_, i64>(24)?))
    })?;
    let mut reviews = Vec::new();
    for row in rows {
        let (record, history_count) = row?;
        let latest_job = record.into_summary()?;
        reviews.push(ProjectPullRequestReviewSummary {
            pr: latest_job.pr,
            latest_job,
            history_count: count_to_usize(history_count, "review history total")?,
        });
    }
    Ok(reviews)
}

fn load_history_count(
    transaction: &rusqlite::Transaction<'_>,
    project_id: ProjectId,
    pr: u64,
) -> Result<usize> {
    let count = transaction.query_row(
        "SELECT COUNT(*) FROM project_review_jobs WHERE project_id = ?1 AND pr = ?2",
        params![project_id.to_string(), u64_to_i64(pr)],
        |row| row.get::<_, i64>(0),
    )?;
    count_to_usize(count, "review history total")
}

fn load_history_page(
    transaction: &rusqlite::Transaction<'_>,
    project_id: ProjectId,
    pr: u64,
    page_size: i64,
    offset: i64,
) -> Result<Vec<ProjectPullRequestReviewHistoryItem>> {
    let sql = format!(
        "SELECT {PROJECT_REVIEW_JOB_COLUMNS},
                EXISTS(
                    SELECT 1 FROM project_review_runs
                    WHERE project_review_runs.job_id = project_review_jobs.id
                ) AS has_attempts
         FROM project_review_jobs
         WHERE project_id = ?1 AND pr = ?2
         ORDER BY created_at DESC, id DESC LIMIT ?3 OFFSET ?4"
    );
    let mut statement = transaction.prepare(&sql)?;
    let rows = statement.query_map(
        params![project_id.to_string(), u64_to_i64(pr), page_size, offset],
        |row| Ok((project_review_job_record(row)?, row.get::<_, bool>(24)?)),
    )?;
    let mut items = Vec::new();
    for row in rows {
        let (record, has_attempts) = row?;
        items.push(ProjectPullRequestReviewHistoryItem {
            job: record.into_summary()?,
            has_attempts,
        });
    }
    Ok(items)
}

fn review_page_offset(page: usize, page_size: usize) -> Result<i64> {
    if page == 0 {
        return Err(StoreError::InvalidConfig(
            "review page must be at least 1".to_string(),
        ));
    }
    if !(1..=MAX_REVIEW_PAGE_SIZE).contains(&page_size) {
        return Err(StoreError::InvalidConfig(format!(
            "review page_size must be between 1 and {MAX_REVIEW_PAGE_SIZE}"
        )));
    }
    let offset = page
        .checked_sub(1)
        .and_then(|index| index.checked_mul(page_size))
        .ok_or_else(|| StoreError::InvalidConfig("review page offset overflow".to_string()))?;
    i64::try_from(offset)
        .map_err(|_| StoreError::InvalidConfig("review page offset exceeds SQLite".to_string()))
}

fn count_to_usize(value: i64, label: &str) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| StoreError::InvalidConfig(format!("{label} is outside usize range")))
}

fn total_pages(total_items: usize, page_size: usize) -> usize {
    total_items.div_ceil(page_size)
}
