use super::*;
use pl_core::{
    context::OpaquePayload,
    persistence::{SessionStoreError, SqliteSessionStore},
    thread::{
        ThreadEffectBatch, ToolDelivery, ToolOutcome,
        cold::ColdStore,
        task::{TaskRecord, TaskStatus},
    },
    tool::ToolOutput,
};
use pretty_assertions::assert_eq;
use rusqlite::Connection;
use std::sync::Arc;
use uuid::Uuid;

async fn old_database(root: &Path, id: Uuid) -> PathBuf {
    let path = root
        .join("sessions")
        .join(id.to_string())
        .join("history.sqlite");
    let owner = id.to_string();
    let store = SqliteSessionStore::open(SqliteSessionOptions { path: path.clone() })
        .await
        .unwrap();
    let effect = ThreadEffectBatch {
        thread_id: owner.clone(),
        sequence: 1,
        tasks: Arc::from([TaskRecord {
            id: "task:call-1".into(),
            call_id: "call-1".into(),
            tool_id: "exec".into(),
            turn_id: "turn-1".into(),
            revision: 2,
            status: TaskStatus::Succeeded,
            cancel_requested: false,
            acknowledgement: None,
            started_sequence: Some(1),
        }]),
        deliveries: Arc::from([ToolDelivery {
            target: Default::default(),
            call_id: "call-1".into(),
            tool_id: "exec".into(),
            output: ToolOutput::new(OpaquePayload::text("原始结果\0完整保留"), vec![]),
            delivered_context: vec![],
            outcome: ToolOutcome::Succeeded,
        }]),
        ..Default::default()
    };
    store
        .register_resource(
            &owner,
            "thread-commit.00000000000000000001",
            effect.encode().unwrap(),
        )
        .unwrap();
    store.flush().await.unwrap();
    store.shutdown().await.unwrap();
    Connection::open(&path).unwrap().execute_batch("DROP TABLE thread_tool_task_resources; DROP TABLE thread_tool_tasks; PRAGMA user_version=11").unwrap();
    path
}

#[tokio::test]
async fn uninitialized_empty_database_is_reported_without_migration_or_rebuilding() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let path = data
        .join("sessions")
        .join(Uuid::from_u128(1).to_string())
        .join("history.sqlite");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, []).unwrap();
    let before = check_pl_sessions(&data).unwrap().sessions.remove(0).before;
    assert_eq!(
        before,
        SessionInspection {
            schema: 0,
            original_tables: vec![],
            indexed_tasks: None,
            indexed_resources: None,
        }
    );
    let report = migrate_pl_sessions(&data, &temp.path().join("backup"))
        .await
        .unwrap();
    assert_eq!(
        (&report.sessions[0].after, &report.sessions[0].backup),
        (&Some(before), &None)
    );
    assert_eq!(fs::read(&path).unwrap(), Vec::<u8>::new());
    let store = SqliteSessionStore::open(SqliteSessionOptions { path: path.clone() })
        .await
        .unwrap();
    store.shutdown().await.unwrap();
    assert_eq!(
        session_database::inspect(&path).unwrap().schema,
        SESSION_SCHEMA_VERSION
    );
    Connection::open(&path)
        .unwrap()
        .execute_batch("PRAGMA user_version=0")
        .unwrap();
    assert!(
        check_pl_sessions(&data).is_err(),
        "有业务表的 schema 0 不属于未初始化空库"
    );
}

#[tokio::test]
async fn native_migration_preserves_original_rows_product_data_and_cold_task_reads() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    fs::create_dir(&data).unwrap();
    let id = Uuid::from_u128(1);
    let path = old_database(&data, id).await;
    let product = data.join("mai-team.sqlite3");
    fs::write(&product, b"product schema 38 stays untouched\0\xff").unwrap();
    let original_file = fs::read(&path).unwrap();
    let before = check_pl_sessions(&data).unwrap();
    assert_eq!(before.sessions[0].before.schema, 11);
    assert_eq!(fs::read(&path).unwrap(), original_file);
    assert!(matches!(
        SqliteSessionStore::open(SqliteSessionOptions { path: path.clone() }).await,
        Err(SessionStoreError::UnsupportedSchema { found: 11, .. })
    ));

    let backup = temp.path().join("backup");
    let report = migrate_pl_sessions(&data, &backup).await.unwrap();
    let entry = &report.sessions[0];
    let after = entry.after.as_ref().unwrap();
    assert_eq!(
        (&after.original_tables, after.schema, after.indexed_tasks),
        (
            &entry.before.original_tables,
            SESSION_SCHEMA_VERSION,
            Some(1)
        )
    );
    assert_eq!(
        session_database::inspect(&backup.join(&entry.database)).unwrap(),
        entry.before
    );
    assert_eq!(
        fs::read(&product).unwrap(),
        b"product schema 38 stays untouched\0\xff"
    );
    let persisted: SessionMigrationReport =
        serde_json::from_slice(&fs::read(backup.join("report.json")).unwrap()).unwrap();
    assert_eq!(persisted, report);
    let store = SqliteSessionStore::open(SqliteSessionOptions { path })
        .await
        .unwrap();
    let fact = ColdStore::read_tool_task(&store, &id.to_string(), "task:call-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fact.task.status, TaskStatus::Succeeded);
    assert!(fact.delivery.is_some());
    store.shutdown().await.unwrap();
    let repeated = migrate_pl_sessions(&data, &temp.path().join("backup-current"))
        .await
        .unwrap();
    assert_eq!(repeated.sessions[0].before, after.clone());
    assert_eq!(repeated.sessions[0].backup, None);
    assert!(
        migrate_pl_sessions(&data, &backup).await.is_err(),
        "不能覆盖旧备份"
    );
}

#[tokio::test]
async fn corrupt_effect_stops_the_batch_and_keeps_failed_step_atomic() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    fs::create_dir(&data).unwrap();
    let first = old_database(&data, Uuid::from_u128(1)).await;
    let broken = old_database(&data, Uuid::from_u128(2)).await;
    let pending = old_database(&data, Uuid::from_u128(3)).await;
    Connection::open(&broken)
        .unwrap()
        .execute("UPDATE session_entries SET payload_hash='damaged'", [])
        .unwrap();
    let backup = temp.path().join("backup");
    assert!(migrate_pl_sessions(&data, &backup).await.is_err());
    let report: SessionMigrationReport =
        serde_json::from_slice(&fs::read(backup.join("report.json")).unwrap()).unwrap();
    assert_eq!(
        report
            .sessions
            .iter()
            .map(|entry| (
                entry.after.is_some(),
                entry.backup.is_some(),
                entry.error.is_some()
            ))
            .collect::<Vec<_>>(),
        vec![
            (true, true, false),
            (false, true, true),
            (false, false, false)
        ]
    );
    assert_eq!(
        (
            session_database::inspect(&first).unwrap().schema,
            session_database::inspect(&broken).unwrap().schema,
            session_database::inspect(&pending).unwrap().schema
        ),
        (SESSION_SCHEMA_VERSION, 11, 11)
    );
    let connection = Connection::open(&broken).unwrap();
    let tables: i64 = connection.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE name IN ('thread_tool_tasks','thread_tool_task_resources')", [], |row| row.get(0)).unwrap();
    assert_eq!(tables, 0);
    let envelope: String = connection
        .query_row("SELECT envelope FROM session_entries", [], |row| row.get(0))
        .unwrap();
    connection
        .execute(
            "UPDATE session_entries SET payload_hash=?",
            [pl_core::context::content_hash(envelope.as_bytes())],
        )
        .unwrap();
    drop(connection);
    let resumed = migrate_pl_sessions(&data, &temp.path().join("retry-backup"))
        .await
        .unwrap();
    assert!(
        resumed
            .sessions
            .iter()
            .all(|entry| entry.after.as_ref().unwrap().schema == SESSION_SCHEMA_VERSION)
    );
}

#[tokio::test]
async fn maintenance_rejects_service_ownership_and_the_native_database_lease() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    fs::create_dir(&data).unwrap();
    let path = old_database(&data, Uuid::from_u128(1)).await;
    let service = DataDirectoryLease::acquire(&data).unwrap();
    assert!(check_pl_sessions(&data).is_err());
    assert!(
        migrate_pl_sessions(&data, &temp.path().join("blocked"))
            .await
            .is_err()
    );
    drop(service);
    let native = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_extension("sqlite.lock"))
        .unwrap();
    native.try_lock().unwrap();
    assert!(
        migrate_pl_sessions(&data, &temp.path().join("native-blocked"))
            .await
            .is_err()
    );
    assert_eq!(session_database::inspect(&path).unwrap().schema, 11);
    drop(native);
    migrate_pl_sessions(&data, &temp.path().join("unlocked"))
        .await
        .unwrap();
}

#[tokio::test]
async fn directory_identity_and_supported_schema_are_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    fs::create_dir(&data).unwrap();
    let path = old_database(&data, Uuid::from_u128(1)).await;
    let renamed = path
        .parent()
        .unwrap()
        .with_file_name(Uuid::from_u128(2).to_string());
    fs::rename(path.parent().unwrap(), &renamed).unwrap();
    assert!(
        check_pl_sessions(&data).is_err(),
        "不能收养其他 Agent 的会话"
    );
    fs::rename(&renamed, path.parent().unwrap()).unwrap();
    Connection::open(&path)
        .unwrap()
        .execute_batch("PRAGMA user_version=10")
        .unwrap();
    assert!(
        check_pl_sessions(&data).is_err(),
        "Mai 本次只承诺 11→12，不隐式迁移更旧版本"
    );
    Connection::open(&path)
        .unwrap()
        .execute_batch("PRAGMA user_version=11")
        .unwrap();
    #[cfg(unix)]
    {
        let outside = temp.path().join("outside.sqlite");
        fs::rename(&path, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(check_pl_sessions(&data).is_err());
        fs::remove_file(&path).unwrap();
        fs::rename(outside, &path).unwrap();
    }
    assert!(
        migrate_pl_sessions(&data, &data.join("backup"))
            .await
            .is_err()
    );
    assert_eq!(session_database::inspect(&path).unwrap().schema, 11);
}
