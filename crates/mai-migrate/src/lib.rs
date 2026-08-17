mod archive;
mod legacy;
mod schema;

#[cfg(test)]
mod tests;

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, TransactionBehavior};
use serde::Serialize;

const LEGACY_SCHEMA: &str = "27";
const THREAD_SCHEMA: &str = "28";
const MERGED_STATE_SCHEMA: &str = "29";
const PR_STATE_SCHEMA: &str = "30";
const TARGET_SCHEMA: &str = "31";

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
        let report = schema::validate_target(&transaction, TARGET_SCHEMA.to_string(), true)?;
        transaction.rollback()?;
        return Ok(report);
    }
    let source_schema = version.clone();
    match version.as_str() {
        LEGACY_SCHEMA => {
            schema::validate_legacy_source(&transaction)?;
            let converted = legacy::convert(&transaction)?;
            schema::install_v28(&transaction, &converted)?;
            schema::validate_v28(&transaction)?;
            schema::install_v29(&transaction)?;
            schema::validate_v29(&transaction)?;
            schema::install_v30(&transaction)?;
        }
        THREAD_SCHEMA => {
            schema::validate_v28(&transaction)?;
            schema::install_v29(&transaction)?;
            schema::validate_v29(&transaction)?;
            schema::install_v30(&transaction)?;
        }
        MERGED_STATE_SCHEMA => {
            schema::validate_v29(&transaction)?;
            schema::install_v30(&transaction)?;
        }
        PR_STATE_SCHEMA => schema::validate_v30(&transaction)?,
        _ => bail!(
            "只支持从 schema {LEGACY_SCHEMA}、{THREAD_SCHEMA}、{MERGED_STATE_SCHEMA} 或 {PR_STATE_SCHEMA} 迁移，当前为 {version}"
        ),
    }
    schema::install_v31(&transaction)?;
    let report = schema::validate_target(&transaction, source_schema, false)?;
    transaction.commit()?;
    Ok(report)
}

pub fn validate_path(path: &Path) -> Result<MigrationReport> {
    let mut connection =
        Connection::open(path).with_context(|| format!("无法打开数据库 {}", path.display()))?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let version = schema::schema_version(&transaction)?;
    let report = match version.as_str() {
        TARGET_SCHEMA => schema::validate_target(&transaction, TARGET_SCHEMA.to_string(), true)?,
        PR_STATE_SCHEMA => {
            schema::validate_v30(&transaction)?;
            schema::report(&transaction, PR_STATE_SCHEMA.to_string(), false)?
        }
        MERGED_STATE_SCHEMA => {
            schema::validate_v29(&transaction)?;
            schema::report(&transaction, MERGED_STATE_SCHEMA.to_string(), false)?
        }
        THREAD_SCHEMA => {
            schema::validate_v28(&transaction)?;
            schema::report(&transaction, THREAD_SCHEMA.to_string(), false)?
        }
        LEGACY_SCHEMA => {
            schema::validate_legacy_source(&transaction)?;
            legacy::validate_convertible(&transaction)?
        }
        _ => bail!(
            "只支持校验 schema {LEGACY_SCHEMA}、{THREAD_SCHEMA}、{MERGED_STATE_SCHEMA}、{PR_STATE_SCHEMA} 或 {TARGET_SCHEMA}，当前为 {version}"
        ),
    };
    transaction.rollback()?;
    Ok(report)
}
