use crate::records::*;
use crate::*;
use rusqlite::Connection;
use rusqlite::{OptionalExtension, params};
use std::time::Duration;
use toasty_driver_sqlite::Sqlite;

pub(crate) const SETTING_SCHEMA_VERSION: &str = "toasty_schema_version";
pub(crate) const SCHEMA_VERSION: &str = "24";
const PREVIOUS_SCHEMA_VERSION: &str = "23";
const LEGACY_REVIEW_SCHEMA_VERSION: &str = "22";
const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";
const SQLITE_POOL_MAX_SIZE: usize = 4;
const SQLITE_POOL_WAIT_TIMEOUT_SECS: u64 = 30;

pub(crate) async fn build_db(path: &Path) -> Result<Db> {
    configure_sqlite_file(path)?;
    let mut builder = Db::builder();
    builder.models(toasty::models!(
        McpServerRecord,
        SettingRecord,
        ProjectRecordRow,
        TaskRecordRow,
        TaskReviewRecord,
        ProjectReviewRunRecord,
        ProjectReviewJobRecord,
        PlanHistoryRecord,
        AgentRecordRow,
        AgentRuntimeStateRecord,
        AgentSessionRecord,
        AgentMessageRecord,
        AgentHistoryRecord,
        AgentPendingInputRecord,
        AgentTurnRecord,
        AgentRuntimeEventRecord,
        AgentRuntimeTraceRecord,
        SessionViewSnapshotRecord,
        SessionEventJournalRecord,
        MaiProductEventRecord,
        AgentLogRecord,
        ToolTraceRecord,
    ));
    builder.max_pool_size(SQLITE_POOL_MAX_SIZE);
    builder.pool_wait_timeout(Some(Duration::from_secs(SQLITE_POOL_WAIT_TIMEOUT_SECS)));
    Ok(builder.build(Sqlite::open(path)).await?)
}

fn configure_sqlite_file(path: &Path) -> Result<()> {
    let connection = Connection::open(path)?;
    let journal_mode: String =
        connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        connection.pragma_update(None, "journal_mode", "WAL")?;
    }
    Ok(())
}

pub(crate) fn migrate_supported_schema(path: &Path) -> Result<bool> {
    let mut connection = Connection::open(path)?;
    let current = connection
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![SETTING_SCHEMA_VERSION],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match current.as_deref() {
        Some(SCHEMA_VERSION) => Ok(true),
        Some(PREVIOUS_SCHEMA_VERSION) => {
            migrate_review_skip_schema(&mut connection)?;
            Ok(true)
        }
        Some(LEGACY_REVIEW_SCHEMA_VERSION) => {
            migrate_review_lifecycle_schema(&mut connection)?;
            Ok(true)
        }
        None | Some(_) => Ok(false),
    }
}

fn migrate_review_skip_schema(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    add_column_if_missing(&transaction, "project_review_jobs", "skip_reason", "TEXT")?;
    transaction.execute(
        "UPDATE settings SET value = ?1 WHERE key = ?2",
        params![SCHEMA_VERSION, SETTING_SCHEMA_VERSION],
    )?;
    transaction.commit()?;
    Ok(())
}

fn migrate_review_lifecycle_schema(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    add_column_if_missing(&transaction, "project_review_runs", "job_id", "TEXT")?;
    add_column_if_missing(
        &transaction,
        "project_review_runs",
        "attempt_index",
        "INTEGER NOT NULL DEFAULT 1",
    )?;
    add_column_if_missing(&transaction, "project_review_runs", "failure_json", "TEXT")?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_review_jobs (
            id TEXT PRIMARY KEY NOT NULL,
            project_id TEXT NOT NULL,
            pr INTEGER NOT NULL,
            head_sha TEXT NOT NULL,
            source TEXT NOT NULL,
            delivery_id TEXT,
            reason TEXT NOT NULL,
            status TEXT NOT NULL,
            attempt_count INTEGER NOT NULL,
            max_attempts INTEGER NOT NULL,
            first_retryable_failure_at TEXT,
            next_attempt_at TEXT,
            reviewer_agent_id TEXT,
            active_run_id TEXT,
            lease_owner TEXT,
            lease_expires_at TEXT,
            failure_json TEXT,
            skip_reason TEXT,
            submission_intent_json TEXT,
            submission_receipt_json TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            finished_at TEXT
        );
        CREATE INDEX IF NOT EXISTS project_review_jobs_project_id_idx
            ON project_review_jobs(project_id);
        CREATE INDEX IF NOT EXISTS project_review_jobs_next_attempt_at_idx
            ON project_review_jobs(next_attempt_at);
        CREATE INDEX IF NOT EXISTS project_review_runs_job_id_idx
            ON project_review_runs(job_id);
        INSERT OR IGNORE INTO project_review_jobs (
            id, project_id, pr, head_sha, source, delivery_id, reason, status,
            attempt_count, max_attempts, first_retryable_failure_at, next_attempt_at,
            reviewer_agent_id, active_run_id, lease_owner, lease_expires_at, failure_json,
            skip_reason,
            submission_intent_json, submission_receipt_json, created_at, updated_at, finished_at
        )
        SELECT id, project_id, COALESCE(pr, 0), 'legacy-unknown', 'legacy', NULL,
            'migrated historical review run',
            CASE status
                WHEN 'completed' THEN 'succeeded'
                WHEN 'cancelled' THEN 'cancelled'
                WHEN 'syncing' THEN 'retry_waiting'
                WHEN 'running' THEN 'retry_waiting'
                ELSE 'failed'
            END,
            1, 5, NULL,
            CASE WHEN status IN ('syncing', 'running')
                THEN strftime('%Y-%m-%dT%H:%M:%fZ','now') ELSE NULL END,
            reviewer_agent_id,
            CASE WHEN status IN ('syncing', 'running') THEN id ELSE NULL END,
            NULL, NULL, NULL, NULL, NULL, NULL, started_at,
            COALESCE(finished_at, started_at), finished_at
        FROM project_review_runs;
        UPDATE project_review_runs SET job_id = id WHERE job_id IS NULL;
        UPDATE project_review_runs SET status = 'interrupted',
            finished_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
            WHERE status IN ('syncing', 'running') AND finished_at IS NULL;
        UPDATE settings SET value = '24' WHERE key = 'toasty_schema_version';",
    )?;
    restore_active_legacy_review_heads(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn restore_active_legacy_review_heads(transaction: &rusqlite::Transaction<'_>) -> Result<()> {
    let migrated = {
        let mut statement = transaction.prepare(
            "SELECT jobs.id, agents.system_prompt
             FROM project_review_jobs AS jobs
             LEFT JOIN agents ON agents.id = jobs.reviewer_agent_id
             WHERE jobs.status = 'retry_waiting' AND jobs.head_sha = 'legacy-unknown'",
        )?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (job_id, system_prompt) in migrated {
        let Some(head_sha) = system_prompt
            .as_deref()
            .and_then(review_head_from_legacy_system_prompt)
        else {
            continue;
        };
        transaction.execute(
            "UPDATE project_review_jobs SET head_sha = ?1 WHERE id = ?2",
            params![head_sha, job_id],
        )?;
    }
    Ok(())
}

fn review_head_from_legacy_system_prompt(system_prompt: &str) -> Option<&str> {
    system_prompt.lines().find_map(|line| {
        let (_, remainder) = line.split_once(" at head `")?;
        let (head_sha, _) = remainder.split_once('`')?;
        (head_sha.len() == 40 && head_sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then_some(head_sha)
    })
}

fn add_column_if_missing(
    transaction: &rusqlite::Transaction<'_>,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let mut statement = transaction.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(());
        }
    }
    transaction.execute_batch(&format!(
        "ALTER TABLE {table} ADD COLUMN {column} {definition}"
    ))?;
    Ok(())
}

pub(crate) fn has_sqlite_header(path: &Path) -> Result<bool> {
    let mut header = [0_u8; 16];
    let bytes_read = std::io::Read::read(&mut std::fs::File::open(path)?, &mut header)?;
    Ok(bytes_read == SQLITE_HEADER.len() && header.as_slice() == SQLITE_HEADER)
}
