use crate::records::*;
use crate::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::time::Duration;
use toasty_driver_sqlite::Sqlite;

pub(crate) const SETTING_SCHEMA_VERSION: &str = "toasty_schema_version";
// 旧数据不保留：mai-store 只持久化 mai 产品数据。移除旧 pl-core Thread runtime
// canonical document、Turn/Item/Notification、runtime/trace event 与 submission 表后，
// schema 直接升级为新的单一版本，不提供旧 runtime 数据的兼容或迁移路径。
// 版本 34 起 `agents` 增加 `review_run_id`：Review Thread 的产品身份必须随 Agent 持久化，
// 加载时只读取该列，不回退为普通项目 Agent。版本 35 起增加 `profile_id` 和
// `workspace_json`：创建时冻结的 Profile 与 workspace assignment 都是产品事实，不随当前
// 配置或 memory 重建；workspace 使用严格 JSON 读写，不做静默回退。
// 版本 36 记录产品 Agent 删除时间，供 PL 会话历史到期清理；不复制 PL 内容。
// 版本 37 用完整 JSON 保存 Review Run 的 PL 用量投影，避免旧计数字段丢失成本与时间。
pub(crate) const SCHEMA_VERSION: &str = "37";
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
        RetiredAgentSessionRecord,
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
