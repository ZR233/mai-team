mod archive;
mod schema;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, TransactionBehavior};
use serde::Serialize;

pub use archive::ArchiveManifest;

const SOURCE_SCHEMA: &str = "31";
pub const TARGET_SCHEMA: &str = "32";

/// schema 31→32 迁移所需的部署事实。
#[derive(Debug, Clone)]
pub struct MigrationOptions {
    pub archive_root: PathBuf,
    pub source_commit: String,
    pub target_commit: String,
}

/// 离线归档和运行态重建结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationReport {
    pub source_schema: String,
    pub target_schema: String,
    pub already_current: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive: Option<ArchiveManifest>,
    pub agents: usize,
    pub product_review_jobs: usize,
    pub product_review_runs: usize,
    pub archived_review_runs: usize,
    pub reset_threads: usize,
    pub reset_turns: usize,
    pub reset_items: usize,
}

/// 对停服后的 schema 31 数据库执行唯一受支持的 PL v2 迁移。
pub fn migrate_path(path: &Path, options: &MigrationOptions) -> Result<MigrationReport> {
    let mut connection = open_database(path)?;
    let version = schema::schema_version(&connection)?;
    if version == TARGET_SCHEMA {
        return schema::validate_target(&connection, true, None);
    }
    if version != SOURCE_SCHEMA {
        bail!("只支持从 schema {SOURCE_SCHEMA} 迁移到 {TARGET_SCHEMA}，当前为 {version}");
    }

    schema::validate_source(&connection)?;
    schema::ensure_quiescent(&connection)?;
    let archive = archive::create(path, &connection, options)?;

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if schema::schema_version(&transaction)? != SOURCE_SCHEMA {
        bail!("归档后源数据库 schema 发生变化，拒绝继续迁移");
    }
    schema::validate_source(&transaction)?;
    schema::ensure_quiescent(&transaction)?;
    let reset = schema::install_v32(&transaction, &archive)?;
    let report = schema::validate_target(&transaction, false, Some((&archive, reset)))?;
    transaction.commit()?;
    Ok(report)
}

/// 只校验 schema 31 的迁移前条件或 schema 32 的目标不变量。
pub fn validate_path(path: &Path) -> Result<MigrationReport> {
    let connection = open_database(path)?;
    match schema::schema_version(&connection)?.as_str() {
        SOURCE_SCHEMA => {
            schema::validate_source(&connection)?;
            schema::ensure_quiescent(&connection)?;
            schema::source_report(&connection)
        }
        TARGET_SCHEMA => schema::validate_target(&connection, true, None),
        version => bail!("只支持校验 schema {SOURCE_SCHEMA} 或 {TARGET_SCHEMA}，当前为 {version}"),
    }
}

fn open_database(path: &Path) -> Result<Connection> {
    let connection =
        Connection::open(path).with_context(|| format!("无法打开数据库 {}", path.display()))?;
    connection.busy_timeout(std::time::Duration::from_secs(30))?;
    Ok(connection)
}
