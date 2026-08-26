use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::{ArchiveManifest, MigrationReport, SOURCE_SCHEMA, TARGET_SCHEMA};

const VERSION_KEY: &str = "toasty_schema_version";
const SKILLS_CONFIG_KEY: &str = "skills_config";
const RUNTIME_TABLES: [&str; 7] = [
    "thread_submissions",
    "thread_notifications",
    "thread_runtime_traces",
    "thread_runtime_events",
    "thread_items",
    "thread_turns",
    "thread_runtime_documents",
];

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ResetCounts {
    threads: usize,
    turns: usize,
    items: usize,
    archived_review_runs: usize,
}

pub(crate) fn schema_version(connection: &Connection) -> Result<String> {
    connection
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![VERSION_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .context("数据库缺少 toasty_schema_version")
}

pub(crate) fn validate_source(connection: &Connection) -> Result<()> {
    if schema_version(connection)? != SOURCE_SCHEMA {
        bail!("源数据库不是 schema {SOURCE_SCHEMA}");
    }
    require_columns(
        connection,
        "agents",
        &["id", "resource_state", "resource_error", "role"],
    )?;
    require_columns(
        connection,
        "projects",
        &["id", "current_reviewer_agent_id", "auto_review_enabled"],
    )?;
    require_columns(
        connection,
        "project_review_jobs",
        &["id", "status", "lease_owner", "lease_expires_at"],
    )?;
    require_columns(
        connection,
        "project_review_runs",
        &["id", "status", "history_json", "finished_at"],
    )?;
    for table in RUNTIME_TABLES {
        if !table_exists(connection, table)? {
            bail!("schema {SOURCE_SCHEMA} 缺少 PL v1 运行表 `{table}`");
        }
    }
    if column_exists(connection, "project_review_runs", "history_status")? {
        bail!("schema {SOURCE_SCHEMA} 意外包含 PL v2 归档标识列");
    }
    Ok(())
}

pub(crate) fn ensure_quiescent(connection: &Connection) -> Result<()> {
    let active_jobs: i64 = connection.query_row(
        "SELECT COUNT(*) FROM project_review_jobs
         WHERE status IN (
            'queued', 'preparing', 'running', 'retry_waiting',
            'submission_pending', 'reconciling'
         ) OR lease_owner IS NOT NULL OR lease_expires_at IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    if active_jobs != 0 {
        bail!("存在 {active_jobs} 个非终态 Review Job 或未清空租约");
    }
    let active_runs: i64 = connection.query_row(
        "SELECT COUNT(*) FROM project_review_runs
         WHERE status IN ('syncing', 'running') OR finished_at IS NULL",
        [],
        |row| row.get(0),
    )?;
    if active_runs != 0 {
        bail!("存在 {active_runs} 个非终态 Review Run");
    }
    let attached_reviewers: i64 = connection.query_row(
        "SELECT COUNT(*) FROM projects WHERE current_reviewer_agent_id IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    if attached_reviewers != 0 {
        bail!("存在 {attached_reviewers} 个尚未清理的项目 Reviewer");
    }
    let active_threads: i64 = connection.query_row(
        "SELECT COUNT(*) FROM thread_runtime_documents
         WHERE json_extract(document_json, '$.snapshot.activeTurnId') IS NOT NULL
            OR COALESCE(json_extract(document_json, '$.snapshot.pendingInputs'), 0) != 0
            OR COALESCE(json_extract(document_json, '$.snapshot.activity'), 'idle') != 'idle'",
        [],
        |row| row.get(0),
    )?;
    if active_threads != 0 {
        bail!("存在 {active_threads} 个未静止的 PL v1 Thread");
    }
    Ok(())
}

pub(crate) fn install_v32(
    transaction: &Transaction<'_>,
    archive: &ArchiveManifest,
) -> Result<ResetCounts> {
    let counts = ResetCounts {
        threads: count(transaction, "thread_runtime_documents")?,
        turns: count(transaction, "thread_turns")?,
        items: count(transaction, "thread_items")?,
        archived_review_runs: count_where(
            transaction,
            "project_review_runs",
            "history_json IS NOT NULL",
        )?,
    };
    transaction.execute_batch(
        "ALTER TABLE project_review_runs
            ADD COLUMN history_status TEXT NOT NULL DEFAULT 'available'
            CHECK (history_status IN ('available', 'pl_v2_archived'));
         ALTER TABLE project_review_runs ADD COLUMN history_archive_id TEXT;
         ALTER TABLE project_review_runs ADD COLUMN history_archived_at TEXT;
         CREATE INDEX index_project_review_runs_history_status
            ON project_review_runs(history_status, history_archived_at);",
    )?;
    transaction.execute(
        "UPDATE project_review_runs
         SET history_status = 'pl_v2_archived',
             history_archive_id = ?1,
             history_archived_at = ?2,
             history_json = NULL
         WHERE history_json IS NOT NULL",
        params![archive.archive_id, archive.created_at],
    )?;
    for table in RUNTIME_TABLES {
        transaction.execute(&format!("DELETE FROM {table}"), [])?;
    }
    // 产品事件只是派生通知，旧 Agent wire payload 不能进入 PL v2 运行期。
    transaction.execute("DELETE FROM product_events", [])?;
    migrate_skills_config(transaction)?;
    transaction.execute(
        "UPDATE settings SET value = ?1 WHERE key = ?2",
        params![TARGET_SCHEMA, VERSION_KEY],
    )?;
    Ok(counts)
}

pub(crate) fn source_report(connection: &Connection) -> Result<MigrationReport> {
    Ok(MigrationReport {
        source_schema: SOURCE_SCHEMA.to_string(),
        target_schema: TARGET_SCHEMA.to_string(),
        already_current: false,
        archive: None,
        agents: count(connection, "agents")?,
        product_review_jobs: count(connection, "project_review_jobs")?,
        product_review_runs: count(connection, "project_review_runs")?,
        archived_review_runs: count_where(
            connection,
            "project_review_runs",
            "history_json IS NOT NULL",
        )?,
        reset_threads: count(connection, "thread_runtime_documents")?,
        reset_turns: count(connection, "thread_turns")?,
        reset_items: count(connection, "thread_items")?,
    })
}

pub(crate) fn validate_target(
    connection: &Connection,
    already_current: bool,
    migrated: Option<(&ArchiveManifest, ResetCounts)>,
) -> Result<MigrationReport> {
    if schema_version(connection)? != TARGET_SCHEMA {
        bail!("目标数据库不是 schema {TARGET_SCHEMA}");
    }
    require_columns(
        connection,
        "project_review_runs",
        &[
            "history_json",
            "history_status",
            "history_archive_id",
            "history_archived_at",
        ],
    )?;
    let invalid_archives: i64 = connection.query_row(
        "SELECT COUNT(*) FROM project_review_runs
         WHERE (history_status = 'pl_v2_archived' AND (
                    history_json IS NOT NULL
                    OR history_archive_id IS NULL
                    OR history_archived_at IS NULL
                ))
            OR (history_status = 'available' AND (
                    history_archive_id IS NOT NULL
                    OR history_archived_at IS NOT NULL
                ))",
        [],
        |row| row.get(0),
    )?;
    if invalid_archives != 0 {
        bail!("有 {invalid_archives} 条 Review Timeline 归档标识不一致");
    }
    let legacy_documents: i64 = connection.query_row(
        "SELECT COUNT(*) FROM thread_runtime_documents
         WHERE json_type(document_json, '$.snapshot.lifecycle') IS NOT NULL
            OR json_type(document_json, '$.snapshot.activity') IS NOT NULL
            OR json_type(document_json, '$.snapshot.activeTurnId') IS NOT NULL
            OR json_type(document_json, '$.snapshot.state.kind') IS NULL",
        [],
        |row| row.get(0),
    )?;
    if legacy_documents != 0 {
        bail!("schema {TARGET_SCHEMA} 中仍有 {legacy_documents} 条 PL v1 runtime document");
    }
    validate_skills_config(connection)?;
    for table in RUNTIME_TABLES {
        if !table_exists(connection, table)? {
            bail!("schema {TARGET_SCHEMA} 缺少 PL v2 运行表 `{table}`");
        }
    }

    let archived_review_runs = count_where(
        connection,
        "project_review_runs",
        "history_status = 'pl_v2_archived'",
    )?;
    let (archive, reset) = migrated.map_or((None, ResetCounts::default()), |(archive, reset)| {
        (Some(archive.clone()), reset)
    });
    if !already_current && archived_review_runs != reset.archived_review_runs {
        bail!(
            "Review Timeline 归档数量不一致: 预期 {}，实际 {archived_review_runs}",
            reset.archived_review_runs
        );
    }
    Ok(MigrationReport {
        source_schema: if already_current {
            TARGET_SCHEMA.to_string()
        } else {
            SOURCE_SCHEMA.to_string()
        },
        target_schema: TARGET_SCHEMA.to_string(),
        already_current,
        archive,
        agents: count(connection, "agents")?,
        product_review_jobs: count(connection, "project_review_jobs")?,
        product_review_runs: count(connection, "project_review_runs")?,
        archived_review_runs,
        reset_threads: reset.threads,
        reset_turns: reset.turns,
        reset_items: reset.items,
    })
}

fn migrate_skills_config(transaction: &Transaction<'_>) -> Result<()> {
    let Some(value) = transaction
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![SKILLS_CONFIG_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    else {
        return Ok(());
    };
    let legacy: serde_json::Value =
        serde_json::from_str(&value).context("schema 31 skills_config 不是合法 JSON")?;
    let entries = legacy
        .get("config")
        .and_then(serde_json::Value::as_array)
        .context("schema 31 skills_config 缺少 config 数组")?;
    let disabled = entries
        .iter()
        .filter(|entry| entry.get("enabled").and_then(serde_json::Value::as_bool) == Some(false))
        .filter_map(|entry| entry.get("name").and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    let migrated = serde_json::to_string(&serde_json::json!({
        "disabled": disabled.into_iter().collect::<Vec<_>>()
    }))?;
    transaction.execute(
        "UPDATE settings SET value = ?1 WHERE key = ?2",
        params![migrated, SKILLS_CONFIG_KEY],
    )?;
    Ok(())
}

fn validate_skills_config(connection: &Connection) -> Result<()> {
    let Some(value) = connection
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![SKILLS_CONFIG_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    else {
        return Ok(());
    };
    let config: serde_json::Value =
        serde_json::from_str(&value).context("schema 32 skills_config 不是合法 JSON")?;
    let object = config
        .as_object()
        .context("schema 32 skills_config 必须是对象")?;
    if object.keys().any(|key| key != "disabled") {
        bail!("schema 32 skills_config 仍包含旧字段");
    }
    let disabled = object
        .get("disabled")
        .and_then(serde_json::Value::as_array)
        .context("schema 32 skills_config 缺少 disabled 数组")?;
    if disabled.iter().any(|name| {
        name.as_str()
            .is_none_or(|name| name.trim().is_empty() || name != name.trim())
    }) {
        bail!("schema 32 skills_config 包含非法 Skill 名称");
    }
    Ok(())
}

fn require_columns(connection: &Connection, table: &str, required: &[&str]) -> Result<()> {
    if !table_exists(connection, table)? {
        bail!("数据库缺少表 `{table}`");
    }
    let columns = table_columns(connection, table)?;
    for column in required {
        if !columns.contains(*column) {
            bail!("表 `{table}` 缺少列 `{column}`");
        }
    }
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(table_columns(connection, table)?.contains(column))
}

fn table_columns(connection: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()
        .map_err(Into::into)
}

fn count(connection: &Connection, table: &str) -> Result<usize> {
    count_where(connection, table, "1 = 1")
}

fn count_where(connection: &Connection, table: &str, condition: &str) -> Result<usize> {
    let value = connection.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {condition}"),
        [],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(usize::try_from(value)?)
}
