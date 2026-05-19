use crate::records::*;
use crate::*;
use std::time::Duration;
use toasty_driver_sqlite::Sqlite;

pub(crate) const SETTING_SCHEMA_VERSION: &str = "toasty_schema_version";
pub(crate) const SCHEMA_VERSION: &str = "17";
const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";
const SQLITE_POOL_MAX_SIZE: usize = 4;
const SQLITE_POOL_WAIT_TIMEOUT_SECS: u64 = 5;

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
    builder.max_pool_size(SQLITE_POOL_MAX_SIZE);
    builder.pool_wait_timeout(Some(Duration::from_secs(SQLITE_POOL_WAIT_TIMEOUT_SECS)));
    Ok(builder.build(Sqlite::open(path)).await?)
}

pub(crate) fn has_sqlite_header(path: &Path) -> Result<bool> {
    let mut header = [0_u8; 16];
    let bytes_read = std::io::Read::read(&mut std::fs::File::open(path)?, &mut header)?;
    Ok(bytes_read == SQLITE_HEADER.len() && header.as_slice() == SQLITE_HEADER)
}
