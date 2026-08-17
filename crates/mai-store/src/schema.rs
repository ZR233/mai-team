use crate::records::*;
use crate::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::time::Duration;
use toasty_driver_sqlite::Sqlite;

pub(crate) const SETTING_SCHEMA_VERSION: &str = "toasty_schema_version";
pub(crate) const SCHEMA_VERSION: &str = "31";
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
        ProjectPullRequestStateRecord,
        ProjectReviewCiWatchRecord,
        ProjectReviewCleanupTaskRecord,
        PlanHistoryRecord,
        AgentRecordRow,
        ThreadRuntimeDocumentRecord,
        ThreadTurnRecord,
        ThreadItemRecord,
        ThreadNotificationRecord,
        ThreadRuntimeEventRecord,
        ThreadRuntimeTraceRecord,
        ThreadSubmissionRecord,
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

/// 只读取 schema 标记；生产运行期不得在这里执行任何迁移或修复。
pub(crate) fn database_schema_version(path: &Path) -> Result<Option<String>> {
    let connection = Connection::open(path)?;
    connection
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![SETTING_SCHEMA_VERSION],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
}

pub(crate) fn has_sqlite_header(path: &Path) -> Result<bool> {
    let mut header = [0_u8; 16];
    let bytes_read = std::io::Read::read(&mut std::fs::File::open(path)?, &mut header)?;
    Ok(bytes_read == SQLITE_HEADER.len() && header.as_slice() == SQLITE_HEADER)
}
