//! 停服产品库的保守迁移；不接触 PL 会话文件或历史目录。
use crate::*;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProductMigrationReport {
    pub database: PathBuf,
    pub schema_before: String,
    pub schema_after: String,
    pub integrity: String,
    pub projects: u64,
    pub agents: u64,
    pub product_events: u64,
    pub review_runs: u64,
    pub backup: Option<PathBuf>,
}

/// 检查产品库完整性与可迁移版本；只读打开，不创建缺失数据库。
pub fn check_product_database(path: &Path) -> Result<ProductMigrationReport> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    inspect(&connection, path)
}

/// 在独立备份之后原子执行 37→38；调用方必须已停止全部产品库写入进程。
pub fn migrate_product_database(path: &Path, backup_path: &Path) -> Result<ProductMigrationReport> {
    ensure_database_offline(path)?;
    if backup_path.exists() || path == backup_path {
        return Err(StoreError::InvalidConfig(
            "备份路径必须不存在且不同于产品库".into(),
        ));
    }
    let mut connection = crate::parent_saves::connection(path)?;
    let before = inspect(&connection, path)?;
    if before.schema_before != "37" {
        return Err(StoreError::InvalidConfig(format!(
            "只接受 schema 37，当前为 {}",
            before.schema_before
        )));
    }
    // create_new 先保留文件名，避免覆盖任何既有备份。
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(backup_path)?;
    connection.backup("main", backup_path, None)?;
    std::fs::File::open(backup_path)?.sync_all()?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)?;
    let version: String = tx.query_row(
        "SELECT value FROM settings WHERE key=?1",
        [crate::schema::SETTING_SCHEMA_VERSION],
        |row| row.get(0),
    )?;
    if version != "37" {
        return Err(StoreError::InvalidConfig(
            "备份期间数据库版本发生变化".into(),
        ));
    }
    crate::reference_schema::create_reference_schema(&tx)?;
    tx.execute(
        "UPDATE settings SET value='38' WHERE key=?1 AND value='37'",
        [crate::schema::SETTING_SCHEMA_VERSION],
    )?;
    let after = inspect(&tx, path)?;
    if (
        before.projects,
        before.agents,
        before.product_events,
        before.review_runs,
    ) != (
        after.projects,
        after.agents,
        after.product_events,
        after.review_runs,
    ) {
        return Err(StoreError::DataIntegrity("迁移改变了既有产品行数".into()));
    }
    tx.commit()?;
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    std::fs::File::open(path)?.sync_all()?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(ProductMigrationReport {
        schema_before: before.schema_before,
        backup: Some(backup_path.to_owned()),
        ..after
    })
}

#[cfg(target_os = "linux")]
fn ensure_database_offline(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let identity = std::fs::metadata(path)?;
    let own_pid = std::process::id().to_string();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let name = entry.file_name();
        let pid = name.to_string_lossy();
        if pid == own_pid || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let descriptors = match std::fs::read_dir(entry.path().join("fd")) {
            Ok(descriptors) => descriptors,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        for descriptor in descriptors {
            let descriptor = descriptor?;
            if let Ok(metadata) = std::fs::metadata(descriptor.path())
                && metadata.dev() == identity.dev()
                && metadata.ino() == identity.ino()
            {
                return Err(StoreError::InvalidConfig(format!(
                    "进程 {pid} 仍持有产品库，请停服后离线迁移"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn ensure_database_offline(_path: &Path) -> Result<()> {
    Ok(())
}

fn inspect(connection: &Connection, path: &Path) -> Result<ProductMigrationReport> {
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(StoreError::DataIntegrity(integrity));
    }
    let violations: Option<String> = connection
        .query_row(
            "SELECT \"table\" FROM pragma_foreign_key_check LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(table) = violations {
        return Err(StoreError::DataIntegrity(format!("外键异常: {table}")));
    }
    let schema: String = connection.query_row(
        "SELECT value FROM settings WHERE key=?1",
        params![crate::schema::SETTING_SCHEMA_VERSION],
        |row| row.get(0),
    )?;
    if !matches!(schema.as_str(), "37" | "38") {
        return Err(StoreError::InvalidConfig(format!("不支持 schema {schema}")));
    }
    let count = |table: &str| -> Result<u64> {
        let count: i64 =
            connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?;
        u64::try_from(count).map_err(|error| StoreError::DataIntegrity(error.to_string()))
    };
    Ok(ProductMigrationReport {
        database: path.to_owned(),
        schema_before: schema.clone(),
        schema_after: schema,
        integrity,
        projects: count("projects")?,
        agents: count("agents")?,
        product_events: count("product_events")?,
        review_runs: count("project_review_runs")?,
        backup: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn offline_migration_preserves_product_rows_and_session_files() {
        let directory = tempfile::tempdir().expect("临时目录");
        let store = MaiStore::open_in_data_dir(directory.path())
            .await
            .expect("创建新库");
        let path = store.path().to_owned();
        drop(store);
        let session = directory.path().join("session-history.fixture");
        std::fs::write(&session, b"PL history must survive\0\xff").expect("会话哨兵");
        let connection = Connection::open(&path).expect("打开库");
        connection.execute_batch("DROP TABLE reference_apply_operations; DROP TABLE agent_reference_bindings; DROP TABLE project_reference_configs; DROP TABLE reference_sources; UPDATE settings SET value='37' WHERE key='toasty_schema_version'; INSERT INTO product_events(sequence,event_json,timestamp) VALUES(1,'{}','2026-10-15T00:00:00Z');").expect("构造 37 库");
        drop(connection);
        let before = check_product_database(&path).expect("检查旧库");
        let backup = directory.path().join("before.sqlite3");
        let after = migrate_product_database(&path, &backup).expect("迁移");
        assert_eq!(
            (
                after.schema_before.as_str(),
                after.schema_after.as_str(),
                after.product_events
            ),
            ("37", "38", before.product_events)
        );
        assert_eq!(
            check_product_database(&backup)
                .expect("检查备份")
                .schema_after,
            "37"
        );
        assert_eq!(
            std::fs::read(session).expect("会话仍在"),
            b"PL history must survive\0\xff"
        );
        assert!(migrate_product_database(&path, &backup).is_err());
        MaiStore::open_in_data_dir(directory.path())
            .await
            .expect("新服务可打开迁移库");
    }
}
