mod archive;
mod legacy;
mod schema;

#[cfg(test)]
mod tests;

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, TransactionBehavior};
use serde::Serialize;

const SOURCE_SCHEMA: &str = "27";
const TARGET_SCHEMA: &str = "28";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationReport {
    pub source_schema: String,
    pub target_schema: String,
    pub already_current: bool,
    pub agents: usize,
    pub canonical_threads: usize,
    pub turns: usize,
    pub items: usize,
    pub archived_review_runs: usize,
}

pub fn migrate_path(path: &Path) -> Result<MigrationReport> {
    let mut connection =
        Connection::open(path).with_context(|| format!("无法打开数据库 {}", path.display()))?;
    connection.busy_timeout(std::time::Duration::from_secs(30))?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version = schema::schema_version(&transaction)?;
    if version == TARGET_SCHEMA {
        let report = schema::validate_target(&transaction, true)?;
        transaction.rollback()?;
        return Ok(report);
    }
    if version != SOURCE_SCHEMA {
        bail!("只支持从 schema {SOURCE_SCHEMA} 迁移，当前为 {version}");
    }

    schema::validate_source(&transaction)?;
    let converted = legacy::convert(&transaction)?;
    schema::install_target(&transaction, &converted)?;
    let report = schema::validate_target(&transaction, false)?;
    transaction.commit()?;
    Ok(report)
}

pub fn validate_path(path: &Path) -> Result<MigrationReport> {
    let mut connection =
        Connection::open(path).with_context(|| format!("无法打开数据库 {}", path.display()))?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let version = schema::schema_version(&transaction)?;
    let report = match version.as_str() {
        TARGET_SCHEMA => schema::validate_target(&transaction, true)?,
        SOURCE_SCHEMA => {
            schema::validate_source(&transaction)?;
            legacy::validate_convertible(&transaction)?
        }
        _ => bail!("只支持校验 schema {SOURCE_SCHEMA} 或 {TARGET_SCHEMA}，当前为 {version}"),
    };
    transaction.rollback()?;
    Ok(report)
}
