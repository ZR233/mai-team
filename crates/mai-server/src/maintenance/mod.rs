//! 产品侧只编排停服所有权、备份和报告；会话 schema 由 PL 原生迁移拥有。

mod data_directory;
mod session_database;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result, ensure};
use pl_core::persistence::{SESSION_SCHEMA_VERSION, SqliteSessionOptions, migration};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub use data_directory::DataDirectoryLease;
pub use session_database::{SessionInspection, TableDigest};

/// 单库的迁移收据；缺少 after 表示尚未完成，错误不改变已经提交的其他库。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMigrationEntry {
    pub database: PathBuf,
    pub before: SessionInspection,
    pub after: Option<SessionInspection>,
    pub backup: Option<PathBuf>,
    pub error: Option<String>,
}

/// 确定顺序的完整会话清单，报告与备份不写入原会话目录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMigrationReport {
    pub target_schema: i64,
    pub sessions: Vec<SessionMigrationEntry>,
}

/// 在数据目录独占所有权下检查当前会话；不迁移、不重建旧库。
pub fn check_pl_sessions(data_root: &Path) -> Result<SessionMigrationReport> {
    let _lease = DataDirectoryLease::acquire(data_root)?;
    inspect_sessions(data_root)
}

fn inspect_sessions(data_root: &Path) -> Result<SessionMigrationReport> {
    let mut sessions = Vec::new();
    for path in session_database::discover(data_root)? {
        let before = session_database::inspect(&path)
            .with_context(|| format!("检查会话失败：{}", path.display()))?;
        sessions.push(SessionMigrationEntry {
            database: path.strip_prefix(data_root)?.to_owned(),
            before,
            after: None,
            backup: None,
            error: None,
        });
    }
    Ok(SessionMigrationReport {
        target_schema: SESSION_SCHEMA_VERSION,
        sessions,
    })
}

/// 为每个待迁移库创建不覆盖的 SQLite 一致备份，再委托 PL 迁移并比较原始行摘要。
///
/// 调用前须停服，backup_root 必须不存在且位于数据目录之外。逐库提交并持久化报告；
/// 失败立即停止，原样保留失败和未处理项，不自动回滚已完成的其他库。
pub async fn migrate_pl_sessions(
    data_root: &Path,
    backup_root: &Path,
) -> Result<SessionMigrationReport> {
    let _lease = DataDirectoryLease::acquire(data_root)?;
    let data_root = fs::canonicalize(data_root)?;
    ensure!(backup_root.is_absolute(), "备份目录必须为绝对路径");
    let backup_parent = fs::canonicalize(backup_root.parent().context("备份目录缺少父目录")?)?;
    ensure!(
        !backup_parent.starts_with(&data_root),
        "备份必须位于数据目录之外"
    );
    let backup_root = backup_parent.join(backup_root.file_name().context("备份目录缺少名称")?);
    let mut report = inspect_sessions(&data_root)?;
    fs::create_dir(&backup_root).context("备份目录必须不存在且父目录可用")?;
    fs::File::open(&backup_parent)?.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&backup_root, fs::Permissions::from_mode(0o700))?;
    }
    save_report(&backup_root, &report)?;
    for index in 0..report.sessions.len() {
        let outcome = async {
            prepare_backup(&data_root, &backup_root, &mut report.sessions[index])?;
            // 先持久化备份身份，再允许 PL 提交 schema 变更。崩溃后可以定位原库的恢复副本。
            save_report(&backup_root, &report)?;
            let entry = &mut report.sessions[index];
            if entry.before.schema == 11 {
                migration::migrate_to_current(
                    SqliteSessionOptions {
                        path: data_root.join(&entry.database),
                    },
                    |_| Ok(()),
                )
                .await?;
                let after = session_database::inspect(&data_root.join(&entry.database))?;
                ensure!(
                    after.schema == SESSION_SCHEMA_VERSION
                        && after.original_tables == entry.before.original_tables,
                    "迁移改变了原始会话数据"
                );
                entry.after = Some(after);
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = outcome {
            report.sessions[index].error = Some(format!("{error:#}"));
            save_report(&backup_root, &report)?;
            return Err(error).with_context(|| {
                format!(
                    "会话迁移停止；收据：{}",
                    backup_root.join("report.json").display()
                )
            });
        }
        save_report(&backup_root, &report)?;
    }
    Ok(report)
}

fn prepare_backup(
    data_root: &Path,
    backup_root: &Path,
    entry: &mut SessionMigrationEntry,
) -> Result<()> {
    let path = data_root.join(&entry.database);
    ensure!(
        session_database::inspect(&path)? == entry.before,
        "检查后会话内容发生变化：{}",
        path.display()
    );
    if entry.before.schema != 11 {
        entry.after = Some(entry.before.clone());
        return Ok(());
    }
    let backup = backup_root.join(&entry.database);
    fs::create_dir_all(backup.parent().context("备份文件缺少父目录")?)?;
    session_database::backup(&path, &backup)?;
    ensure!(
        session_database::inspect(&backup)? == entry.before,
        "备份与检查时的会话不一致"
    );
    for parent in backup.parent().into_iter().flat_map(Path::ancestors) {
        fs::File::open(parent)?.sync_all()?;
        if parent == backup_root {
            break;
        }
    }
    entry.backup = Some(entry.database.clone());
    Ok(())
}

fn save_report(root: &Path, report: &SessionMigrationReport) -> Result<()> {
    let pending = root.join("report.json.pending");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending)?;
    serde_json::to_writer_pretty(&mut file, report)?;
    file.sync_all()?;
    drop(file);
    fs::rename(pending, root.join("report.json"))?;
    fs::File::open(root)?.sync_all()?;
    Ok(())
}
