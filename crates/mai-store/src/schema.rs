use crate::records::*;
use crate::*;
use rusqlite::Connection as SqliteConnection;
use toasty_driver_sqlite::Sqlite;

pub(crate) const SETTING_SCHEMA_VERSION: &str = "toasty_schema_version";
pub(crate) const SCHEMA_VERSION: &str = "16";
const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";

pub(crate) async fn build_db(path: &Path) -> Result<Db> {
    let mut builder = Db::builder();
    builder.models(toasty::models!(
        McpServerRecord,
        SettingRecord,
        ProjectRecordRow,
        TaskRecordRow,
        TaskReviewRecord,
        ProjectReviewRunRecord,
        PlanHistoryRecord,
        AgentRecordRow,
        AgentSessionRecord,
        AgentMessageRecord,
        AgentHistoryRecord,
        ServiceEventRecord,
        AgentLogRecord,
        ToolTraceRecord,
    ));
    builder.max_pool_size(1);
    Ok(builder.build(Sqlite::open(path)).await?)
}

pub(crate) fn migrate_to_current(path: &Path) -> Result<()> {
    let conn = SqliteConnection::open(path)?;
    if !sqlite_column_exists(&conn, "agents", "project_id")? {
        conn.execute("ALTER TABLE agents ADD COLUMN project_id TEXT", [])?;
    }
    conn.execute(
        "CREATE TABLE IF NOT EXISTS projects (
            id TEXT PRIMARY KEY NOT NULL,
            name TEXT NOT NULL,
            status TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            repository_full_name TEXT NOT NULL DEFAULT '',
            git_account_id TEXT,
            repository_id BIGINT NOT NULL,
            installation_id BIGINT NOT NULL,
            installation_account TEXT NOT NULL,
            branch TEXT NOT NULL DEFAULT '',
            docker_image TEXT NOT NULL,
            clone_status TEXT NOT NULL,
            maintainer_agent_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            last_error TEXT,
            auto_review_enabled BOOLEAN NOT NULL DEFAULT 0,
            reviewer_extra_prompt TEXT,
            review_status TEXT NOT NULL DEFAULT 'disabled',
            current_reviewer_agent_id TEXT,
            last_review_started_at TEXT,
            last_review_finished_at TEXT,
            next_review_at TEXT,
            last_review_outcome TEXT,
            review_last_error TEXT
        )",
        [],
    )?;
    if !sqlite_column_exists(&conn, "projects", "repository_full_name")? {
        conn.execute(
            "ALTER TABLE projects ADD COLUMN repository_full_name TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    if !sqlite_column_exists(&conn, "projects", "git_account_id")? {
        conn.execute("ALTER TABLE projects ADD COLUMN git_account_id TEXT", [])?;
    }
    if !sqlite_column_exists(&conn, "projects", "branch")? {
        conn.execute(
            "ALTER TABLE projects ADD COLUMN branch TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    ensure_project_review_columns(&conn)?;
    conn.execute(
        "UPDATE projects
            SET repository_full_name = owner || '/' || repo
          WHERE repository_full_name = ''",
        [],
    )?;
    drop_project_path_columns(&conn)?;
    ensure_project_review_runs_table(&conn)?;
    ensure_agent_log_tables(&conn)?;
    Ok(())
}

fn drop_project_path_columns(conn: &SqliteConnection) -> Result<()> {
    if !sqlite_column_exists(conn, "projects", "project_path")?
        && !sqlite_column_exists(conn, "projects", "workspace_path")?
    {
        return Ok(());
    }
    conn.execute(
        "CREATE TABLE projects_v12 (
            id TEXT PRIMARY KEY NOT NULL,
            name TEXT NOT NULL,
            status TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            repository_full_name TEXT NOT NULL DEFAULT '',
            git_account_id TEXT,
            repository_id BIGINT NOT NULL,
            installation_id BIGINT NOT NULL,
            installation_account TEXT NOT NULL,
            branch TEXT NOT NULL DEFAULT '',
            docker_image TEXT NOT NULL,
            clone_status TEXT NOT NULL,
            maintainer_agent_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            last_error TEXT,
            auto_review_enabled BOOLEAN NOT NULL DEFAULT 0,
            reviewer_extra_prompt TEXT,
            review_status TEXT NOT NULL DEFAULT 'disabled',
            current_reviewer_agent_id TEXT,
            last_review_started_at TEXT,
            last_review_finished_at TEXT,
            next_review_at TEXT,
            last_review_outcome TEXT,
            review_last_error TEXT
        )",
        [],
    )?;
    conn.execute(
        "INSERT INTO projects_v12 (
            id,
            name,
            status,
            owner,
            repo,
            repository_full_name,
            git_account_id,
            repository_id,
            installation_id,
            installation_account,
            branch,
            docker_image,
            clone_status,
            maintainer_agent_id,
            created_at,
            updated_at,
            last_error,
            auto_review_enabled,
            reviewer_extra_prompt,
            review_status,
            current_reviewer_agent_id,
            last_review_started_at,
            last_review_finished_at,
            next_review_at,
            last_review_outcome,
            review_last_error
        )
        SELECT
            id,
            name,
            status,
            owner,
            repo,
            repository_full_name,
            git_account_id,
            repository_id,
            installation_id,
            installation_account,
            branch,
            docker_image,
            clone_status,
            maintainer_agent_id,
            created_at,
            updated_at,
            last_error,
            auto_review_enabled,
            reviewer_extra_prompt,
            review_status,
            current_reviewer_agent_id,
            last_review_started_at,
            last_review_finished_at,
            next_review_at,
            last_review_outcome,
            review_last_error
        FROM projects",
        [],
    )?;
    conn.execute("DROP TABLE projects", [])?;
    conn.execute("ALTER TABLE projects_v12 RENAME TO projects", [])?;
    Ok(())
}

fn ensure_project_review_columns(conn: &SqliteConnection) -> Result<()> {
    let columns = [
        (
            "auto_review_enabled",
            "ALTER TABLE projects ADD COLUMN auto_review_enabled BOOLEAN NOT NULL DEFAULT 0",
        ),
        (
            "reviewer_extra_prompt",
            "ALTER TABLE projects ADD COLUMN reviewer_extra_prompt TEXT",
        ),
        (
            "review_status",
            "ALTER TABLE projects ADD COLUMN review_status TEXT NOT NULL DEFAULT 'disabled'",
        ),
        (
            "current_reviewer_agent_id",
            "ALTER TABLE projects ADD COLUMN current_reviewer_agent_id TEXT",
        ),
        (
            "last_review_started_at",
            "ALTER TABLE projects ADD COLUMN last_review_started_at TEXT",
        ),
        (
            "last_review_finished_at",
            "ALTER TABLE projects ADD COLUMN last_review_finished_at TEXT",
        ),
        (
            "next_review_at",
            "ALTER TABLE projects ADD COLUMN next_review_at TEXT",
        ),
        (
            "last_review_outcome",
            "ALTER TABLE projects ADD COLUMN last_review_outcome TEXT",
        ),
        (
            "review_last_error",
            "ALTER TABLE projects ADD COLUMN review_last_error TEXT",
        ),
    ];
    for (column, statement) in columns {
        if !sqlite_column_exists(conn, "projects", column)? {
            conn.execute(statement, [])?;
        }
    }
    Ok(())
}

fn ensure_project_review_runs_table(conn: &SqliteConnection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS project_review_runs (
            id TEXT PRIMARY KEY NOT NULL,
            project_id TEXT NOT NULL,
            reviewer_agent_id TEXT,
            turn_id TEXT,
            started_at TEXT NOT NULL,
            finished_at TEXT,
            status TEXT NOT NULL,
            outcome TEXT,
            pr BIGINT,
            summary TEXT,
            error TEXT,
            messages_json TEXT NOT NULL DEFAULT '[]',
            events_json TEXT NOT NULL DEFAULT '[]'
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_project_review_runs_project_id
            ON project_review_runs(project_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_project_review_runs_started_at
            ON project_review_runs(started_at)",
        [],
    )?;
    Ok(())
}

fn ensure_agent_log_tables(conn: &SqliteConnection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS agent_log_entries (
            id TEXT PRIMARY KEY NOT NULL,
            agent_id TEXT NOT NULL,
            session_id TEXT,
            turn_id TEXT,
            level TEXT NOT NULL,
            category TEXT NOT NULL,
            message TEXT NOT NULL,
            details_json TEXT NOT NULL DEFAULT '{}',
            timestamp TEXT NOT NULL
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_agent_log_entries_agent_id
            ON agent_log_entries(agent_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_agent_log_entries_session_id
            ON agent_log_entries(session_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_agent_log_entries_turn_id
            ON agent_log_entries(turn_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_agent_log_entries_category
            ON agent_log_entries(category)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_agent_log_entries_timestamp
            ON agent_log_entries(timestamp)",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS tool_trace_records (
            id TEXT PRIMARY KEY NOT NULL,
            call_id TEXT NOT NULL,
            agent_id TEXT NOT NULL,
            session_id TEXT,
            turn_id TEXT,
            tool_name TEXT NOT NULL,
            arguments_json TEXT NOT NULL DEFAULT '{}',
            output TEXT NOT NULL DEFAULT '',
            success BOOLEAN NOT NULL DEFAULT 0,
            duration_ms BIGINT,
            started_at TEXT NOT NULL,
            completed_at TEXT,
            output_preview TEXT NOT NULL DEFAULT '',
            output_artifacts_json TEXT NOT NULL DEFAULT '[]'
        )",
        [],
    )?;
    ensure_tool_trace_id_column(conn)?;
    ensure_tool_trace_output_artifacts_column(conn)?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tool_trace_records_call_id
            ON tool_trace_records(call_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tool_trace_records_agent_id
            ON tool_trace_records(agent_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tool_trace_records_session_id
            ON tool_trace_records(session_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tool_trace_records_turn_id
            ON tool_trace_records(turn_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tool_trace_records_started_at
            ON tool_trace_records(started_at)",
        [],
    )?;
    Ok(())
}

fn ensure_tool_trace_id_column(conn: &SqliteConnection) -> Result<()> {
    if !sqlite_table_exists(conn, "tool_trace_records")?
        || sqlite_column_exists(conn, "tool_trace_records", "id")?
    {
        return Ok(());
    }

    conn.execute(
        "ALTER TABLE tool_trace_records RENAME TO tool_trace_records_v15",
        [],
    )?;
    conn.execute(
        "CREATE TABLE tool_trace_records (
            id TEXT PRIMARY KEY NOT NULL,
            call_id TEXT NOT NULL,
            agent_id TEXT NOT NULL,
            session_id TEXT,
            turn_id TEXT,
            tool_name TEXT NOT NULL,
            arguments_json TEXT NOT NULL DEFAULT '{}',
            output TEXT NOT NULL DEFAULT '',
            success BOOLEAN NOT NULL DEFAULT 0,
            duration_ms BIGINT,
            started_at TEXT NOT NULL,
            completed_at TEXT,
            output_preview TEXT NOT NULL DEFAULT '',
            output_artifacts_json TEXT NOT NULL DEFAULT '[]'
        )",
        [],
    )?;
    conn.execute(
        "INSERT INTO tool_trace_records (
            id,
            call_id,
            agent_id,
            session_id,
            turn_id,
            tool_name,
            arguments_json,
            output,
            success,
            duration_ms,
            started_at,
            completed_at,
            output_preview,
            output_artifacts_json
        )
        SELECT
            agent_id || ':' || COALESCE(session_id, '') || ':' || COALESCE(turn_id, '') || ':' || call_id,
            call_id,
            agent_id,
            session_id,
            turn_id,
            tool_name,
            arguments_json,
            output,
            success,
            duration_ms,
            started_at,
            completed_at,
            output_preview,
            '[]'
        FROM tool_trace_records_v15",
        [],
    )?;
    conn.execute("DROP TABLE tool_trace_records_v15", [])?;
    Ok(())
}

fn ensure_tool_trace_output_artifacts_column(conn: &SqliteConnection) -> Result<()> {
    if !sqlite_table_exists(conn, "tool_trace_records")?
        || sqlite_column_exists(conn, "tool_trace_records", "output_artifacts_json")?
    {
        return Ok(());
    }
    conn.execute(
        "ALTER TABLE tool_trace_records
            ADD COLUMN output_artifacts_json TEXT NOT NULL DEFAULT '[]'",
        [],
    )?;
    Ok(())
}

pub(crate) fn sqlite_column_exists(
    conn: &SqliteConnection,
    table: &str,
    column: &str,
) -> Result<bool> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn sqlite_table_exists(conn: &SqliteConnection, table: &str) -> Result<bool> {
    let mut statement =
        conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1")?;
    let mut rows = statement.query([table])?;
    Ok(rows.next()?.is_some())
}

pub(crate) fn has_sqlite_header(path: &Path) -> Result<bool> {
    let mut header = [0_u8; 16];
    let bytes_read = std::io::Read::read(&mut std::fs::File::open(path)?, &mut header)?;
    Ok(bytes_read == SQLITE_HEADER.len() && header.as_slice() == SQLITE_HEADER)
}
