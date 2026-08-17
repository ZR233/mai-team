use std::path::Path;

use pl_protocol::{
    AgentMessageChannel, ThreadContextDisposition, ThreadItem, ThreadItemContent, ThreadItemStatus,
    ThreadToolCall, ThreadTurnHistory, Turn, TurnState,
};
use pretty_assertions::assert_eq;
use rusqlite::{Connection, OptionalExtension, params};
use tempfile::TempDir;

use crate::{migrate_path, validate_path};

const AGENT_ID: &str = "7f16fb92-8166-45e1-b6b4-29c6a9536a10";
const SESSION_ID: &str = "legacy-session";
const STARTED_AT: &str = "2026-08-01T01:02:03Z";

#[test]
fn v27_migration_is_atomic_and_review_history_is_deeply_equal() {
    let (directory, path) = fixture(false);
    let validation = validate_path(&path).expect("validate v27 fixture");
    assert_eq!(validation.source_schema, "27");
    assert_eq!(validation.target_schema, "31");
    assert!(!validation.already_current);
    let report = migrate_path(&path).expect("migrate fixture");
    assert_eq!(report.source_schema, "27");
    assert_eq!(report.target_schema, "31");
    assert_eq!(report.canonical_threads, 1);
    assert_eq!(report.turns, 1);
    assert_eq!(report.items, 1);
    assert_eq!(report.archived_review_runs, 1);

    let connection = Connection::open(&path).expect("open migrated fixture");
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM product_events", [], |row| row
                .get::<_, i64>(0))
            .expect("count migrated product events"),
        0
    );
    let history_json: String = connection
        .query_row(
            "SELECT history_json FROM project_review_runs WHERE id = 'review-run'",
            [],
            |row| row.get(0),
        )
        .expect("load review history");
    let timestamp = chrono::DateTime::parse_from_rfc3339(STARTED_AT)
        .expect("timestamp")
        .timestamp();
    assert_eq!(
        serde_json::from_str::<ThreadTurnHistory>(&history_json).expect("typed history"),
        ThreadTurnHistory {
            turn: Turn {
                id: "review-turn".to_string(),
                thread_id: AGENT_ID.to_string(),
                state: TurnState::Completed,
                failure: None,
                started_at: Some(timestamp),
                updated_at: timestamp,
                completed_at: Some(timestamp),
            },
            items: vec![ThreadItem {
                id: "review:review-turn:message:0".to_string(),
                thread_id: AGENT_ID.to_string(),
                turn_id: "review-turn".to_string(),
                ordinal: 0,
                revision: 1,
                status: ThreadItemStatus::Completed,
                created_at: timestamp,
                updated_at: timestamp,
                completed_at: Some(timestamp),
                error: None,
                content: ThreadItemContent::AgentMessage {
                    channel: AgentMessageChannel::Final,
                    text: "review complete".to_string(),
                },
                usage: None,
            }],
            context_disposition: ThreadContextDisposition::RolledBack,
        }
    );
    drop(connection);

    let repeated = migrate_path(&path).expect("repeat only validates");
    assert!(repeated.already_current);
    assert_eq!(validate_path(&path).expect("validate current"), repeated);
    drop(directory);
}

#[test]
fn v28_migration_adds_pull_request_lifecycle_state_table() {
    let (_directory, path) = fixture(false);
    migrate_fixture_to_v28(&path);

    let report = migrate_path(&path).expect("migrate v28 fixture");
    assert_eq!(report.source_schema, "28");
    assert_eq!(report.target_schema, "31");
    assert!(!report.already_current);
    let connection = Connection::open(&path).expect("open v31 fixture");
    let version: String = connection
        .query_row(
            "SELECT value FROM settings WHERE key = 'toasty_schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("schema version");
    assert_eq!(version, "31");
    let columns = connection
        .prepare("SELECT name FROM pragma_table_info('project_pull_request_states')")
        .expect("prepare terminal state columns")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query terminal state columns")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect merged columns");
    assert_eq!(
        columns,
        vec![
            "project_id",
            "pr",
            "state",
            "state_changed_at",
            "detected_at",
        ]
    );
    let primary_key = connection
        .prepare(
            "SELECT name FROM pragma_table_info('project_pull_request_states') \
             WHERE pk > 0 ORDER BY pk",
        )
        .expect("prepare terminal state primary key")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query terminal state primary key")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect merged primary key");
    assert_eq!(primary_key, vec!["project_id", "pr"]);
    let project_index: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'index' \
               AND name = 'index_project_pull_request_states_by_project'",
            [],
            |row| row.get(0),
        )
        .expect("terminal state project index");
    assert_eq!(project_index, 1);
}

#[test]
fn v29_migration_preserves_merged_state_in_unified_lifecycle_table() {
    let (_directory, path) = fixture(false);
    migrate_fixture_to_v29(&path);

    let report = migrate_path(&path).expect("migrate v29 fixture");
    assert_eq!(report.source_schema, "29");
    assert_eq!(report.target_schema, "31");
    let connection = Connection::open(&path).expect("open v31 fixture");
    let terminal_state = connection
        .query_row(
            "SELECT project_id, pr, state, state_changed_at, detected_at
             FROM project_pull_request_states",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .expect("migrated terminal state");
    assert_eq!(
        terminal_state,
        (
            "project-1".to_string(),
            42,
            "merged".to_string(),
            STARTED_AT.to_string(),
            STARTED_AT.to_string(),
        )
    );
    let old_table: Option<String> = connection
        .query_row(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name = 'project_merged_pull_requests'",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("old table lookup");
    assert_eq!(old_table, None);
}

#[test]
fn v30_migration_restores_archived_tool_call_identity() {
    let (_directory, path) = fixture(false);
    migrate_fixture_to_v30(&path);
    let connection = Connection::open(&path).expect("open v30 fixture");
    let raw: String = connection
        .query_row(
            "SELECT history_json FROM project_review_runs WHERE id = 'review-run'",
            [],
            |row| row.get(0),
        )
        .expect("load v30 review history");
    let mut history = serde_json::from_str::<serde_json::Value>(&raw).expect("history JSON");
    history["items"][0]["content"] = serde_json::json!({
        "type": "toolCall",
        "tool": {
            "toolCallId": "review:review-turn:message:0",
            "name": "archived_tool_output",
            "result": "review complete",
            "timedOut": false
        }
    });
    connection
        .execute(
            "UPDATE project_review_runs SET history_json = ?1 WHERE id = 'review-run'",
            [serde_json::to_string(&history).expect("serialize legacy history")],
        )
        .expect("write legacy v30 review history");
    drop(connection);

    let report = migrate_path(&path).expect("migrate legacy v30 tool history");
    assert_eq!(report.source_schema, "30");
    assert_eq!(report.target_schema, "31");
    let connection = Connection::open(&path).expect("open v31 fixture");
    let migrated_raw: String = connection
        .query_row(
            "SELECT history_json FROM project_review_runs WHERE id = 'review-run'",
            [],
            |row| row.get(0),
        )
        .expect("load migrated review history");
    let migrated =
        serde_json::from_str::<ThreadTurnHistory>(&migrated_raw).expect("typed migrated history");
    assert_eq!(
        migrated.items[0].content,
        ThreadItemContent::ToolCall {
            tool: ThreadToolCall {
                tool_call_id: "review:review-turn:message:0".to_string(),
                call_id: "review:review-turn:message:0".to_string(),
                provider_item_id: None,
                name: "archived_tool_output".to_string(),
                arguments: String::new(),
                result: Some("review complete".to_string()),
                output_artifacts: Vec::new(),
                exit_code: None,
                timed_out: false,
                working_directory: None,
                denial_reason: None,
            },
        }
    );
    drop(connection);

    let repeated = migrate_path(&path).expect("repeat only validates");
    assert!(repeated.already_current);
    let connection = Connection::open(&path).expect("open repeated fixture");
    let repeated_raw: String = connection
        .query_row(
            "SELECT history_json FROM project_review_runs WHERE id = 'review-run'",
            [],
            |row| row.get(0),
        )
        .expect("load repeated review history");
    assert_eq!(repeated_raw, migrated_raw);
}

#[test]
fn v30_migration_restores_nested_active_activity_shape() {
    let (_directory, path) = fixture(false);
    migrate_fixture_to_v30(&path);
    let connection = Connection::open(&path).expect("open v30 fixture");
    let raw: String = connection
        .query_row(
            "SELECT document_json FROM thread_runtime_documents WHERE thread_id = ?1",
            [AGENT_ID],
            |row| row.get(0),
        )
        .expect("load v30 runtime document");
    let mut document = serde_json::from_str::<serde_json::Value>(&raw).expect("document JSON");
    document["snapshot"]["activity"] = serde_json::Value::String("running".to_string());
    connection
        .execute(
            "UPDATE thread_runtime_documents SET document_json = ?1 WHERE thread_id = ?2",
            params![
                serde_json::to_string(&document).expect("serialize legacy document"),
                AGENT_ID
            ],
        )
        .expect("write legacy v30 runtime document");
    drop(connection);

    migrate_path(&path).expect("migrate legacy v30 runtime activity");
    let connection = Connection::open(&path).expect("open v31 fixture");
    let migrated_raw: String = connection
        .query_row(
            "SELECT document_json FROM thread_runtime_documents WHERE thread_id = ?1",
            [AGENT_ID],
            |row| row.get(0),
        )
        .expect("load migrated runtime document");
    let migrated =
        serde_json::from_str::<serde_json::Value>(&migrated_raw).expect("migrated document JSON");
    assert_eq!(
        migrated["snapshot"]["activity"],
        serde_json::to_value(pl_core::AgentActivityState::Active(
            pl_core::ActiveKind::Running
        ))
        .expect("serialize current activity")
    );
}

#[test]
fn v28_migration_preserves_review_runs_without_archive_history() {
    let (_directory, path) = fixture(false);
    migrate_fixture_to_v28(&path);
    let connection = Connection::open(&path).expect("open v28 fixture");
    connection
        .execute("UPDATE project_review_runs SET history_json = NULL", [])
        .expect("remove historical archive");
    drop(connection);

    let report = migrate_path(&path).expect("migrate v28 fixture with missing archive");
    assert_eq!(report.source_schema, "28");
    assert_eq!(report.target_schema, "31");
    assert_eq!(report.archived_review_runs, 0);
    let connection = Connection::open(&path).expect("open v31 fixture");
    let history: Option<String> = connection
        .query_row("SELECT history_json FROM project_review_runs", [], |row| {
            row.get(0)
        })
        .expect("load preserved review run");
    assert_eq!(history, None);
}

#[test]
fn v28_migration_failure_rolls_back_schema_and_table() {
    let (_directory, path) = fixture(false);
    migrate_fixture_to_v28(&path);
    let connection = Connection::open(&path).expect("open v28 fixture");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_v29_version
             BEFORE UPDATE OF value ON settings
             WHEN OLD.value = '28'
             BEGIN SELECT RAISE(ABORT, 'injected v29 migration failure'); END;",
        )
        .expect("install failure trigger");
    drop(connection);

    let error = migrate_path(&path).expect_err("v29 migration must fail");
    assert!(error.to_string().contains("injected v29 migration failure"));
    let connection = Connection::open(&path).expect("inspect rollback");
    let version: String = connection
        .query_row(
            "SELECT value FROM settings WHERE key = 'toasty_schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("schema version");
    assert_eq!(version, "28");
    let table = connection
        .query_row(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name = 'project_merged_pull_requests'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .expect("merged table lookup");
    assert_eq!(table, None);
}

#[test]
fn malformed_v29_merged_table_is_rejected_as_not_current() {
    let (_directory, path) = fixture(false);
    migrate_fixture_to_v29(&path);
    let connection = Connection::open(&path).expect("open v29 fixture");
    connection
        .execute_batch(
            "DROP TABLE project_merged_pull_requests;
             CREATE TABLE project_merged_pull_requests (
                project_id TEXT NOT NULL,
                pr BIGINT NOT NULL,
                merged_at TEXT NOT NULL,
                detected_at TEXT NOT NULL,
                PRIMARY KEY (project_id)
             );",
        )
        .expect("corrupt merged table");
    drop(connection);

    let error = migrate_path(&path).expect_err("malformed v29 table must fail validation");
    assert!(error.to_string().contains("project_merged_pull_requests"));
}

#[test]
fn v29_migration_failure_rolls_back_schema_and_merged_data() {
    let (_directory, path) = fixture(false);
    migrate_fixture_to_v29(&path);
    let connection = Connection::open(&path).expect("open v29 fixture");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_v30_version
             BEFORE UPDATE OF value ON settings
             WHEN OLD.value = '29'
             BEGIN SELECT RAISE(ABORT, 'injected v30 migration failure'); END;",
        )
        .expect("install failure trigger");
    drop(connection);

    let error = migrate_path(&path).expect_err("v30 migration must fail");
    assert!(error.to_string().contains("injected v30 migration failure"));
    let connection = Connection::open(&path).expect("inspect rollback");
    let version: String = connection
        .query_row(
            "SELECT value FROM settings WHERE key = 'toasty_schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("schema version");
    assert_eq!(version, "29");
    let merged_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM project_merged_pull_requests",
            [],
            |row| row.get(0),
        )
        .expect("merged rows");
    assert_eq!(merged_count, 1);
    let terminal_table: Option<String> = connection
        .query_row(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name = 'project_pull_request_states'",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("terminal table lookup");
    assert_eq!(terminal_table, None);
}

#[test]
fn injected_write_failure_rolls_back_the_entire_migration() {
    let (_directory, path) = fixture(true);
    let error = migrate_path(&path).expect_err("trigger must abort migration");
    assert!(error.to_string().contains("injected migration failure"));

    let connection = Connection::open(path).expect("inspect rolled back fixture");
    let version: String = connection
        .query_row(
            "SELECT value FROM settings WHERE key = 'toasty_schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("schema marker");
    assert_eq!(version, "27");
    let target_table = connection
        .query_row(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name = 'thread_runtime_documents'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .expect("target table lookup");
    assert_eq!(target_table, None);
    let legacy_column: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('agents') WHERE name = 'runtime_agent_id'",
            [],
            |row| row.get(0),
        )
        .expect("legacy column");
    assert_eq!(legacy_column, 1);
}

#[test]
fn nonterminal_review_run_blocks_offline_migration() {
    let (_directory, path) = fixture(false);
    let connection = Connection::open(&path).expect("open fixture");
    connection
        .execute(
            "UPDATE project_review_runs SET status = 'running' WHERE id = 'review-run'",
            [],
        )
        .expect("mark review running");
    drop(connection);

    let error = migrate_path(&path).expect_err("running review must block migration");
    assert!(error.to_string().contains("非终态 review run"));
}

#[test]
fn unknown_review_status_is_rejected_instead_of_projected() {
    let timestamp = chrono::DateTime::parse_from_rfc3339(STARTED_AT)
        .expect("timestamp")
        .timestamp();
    let error = crate::archive::review_history(&crate::archive::ReviewArchiveSource {
        run_id: "run".to_string(),
        reviewer_thread_id: Some(AGENT_ID.to_string()),
        requested_turn_id: Some("turn".to_string()),
        status: "unknown".to_string(),
        started_at: timestamp,
        finished_at: Some(timestamp),
        messages_json: "[]".to_string(),
        events_json: "[]".to_string(),
    })
    .expect_err("unknown status must fail");
    assert!(error.to_string().contains("未知历史 review 状态"));
}

#[test]
fn current_schema_rejects_legacy_product_event_payloads() {
    let (_directory, path) = fixture(false);
    migrate_path(&path).expect("migrate fixture");
    let connection = Connection::open(&path).expect("open migrated fixture");
    connection
        .execute(
            "INSERT INTO product_events VALUES (47267, ?1, NULL, ?2)",
            params![STARTED_AT, legacy_product_event_json()],
        )
        .expect("insert incompatible event");
    drop(connection);

    let error = validate_path(&path).expect_err("legacy event must fail target validation");
    let message = format!("{error:#}");
    assert!(message.contains("product event 47267 不符合当前协议"));
    assert!(message.contains("missing field `thread_id`"));
}

#[test]
fn nested_agent_depth_is_derived_from_the_complete_parent_chain() {
    let (_directory, path) = fixture(false);
    let child = "eab03925-fbda-44ea-87d3-44078a39acb3";
    let grandchild = "d6c51c0c-e68a-4cf0-bebd-a20cb6a501be";
    let connection = Connection::open(&path).expect("open fixture");
    connection
        .execute(
            "INSERT INTO agents VALUES (?1, ?1, ?2, 'executor', ?3, ?3)",
            params![child, AGENT_ID, STARTED_AT],
        )
        .expect("child");
    connection
        .execute(
            "INSERT INTO agents VALUES (?1, ?1, ?2, 'executor', ?3, ?3)",
            params![grandchild, child, STARTED_AT],
        )
        .expect("grandchild");
    drop(connection);

    migrate_path(&path).expect("migrate nested agents");
    let connection = Connection::open(path).expect("open migrated fixture");
    let document: String = connection
        .query_row(
            "SELECT document_json FROM thread_runtime_documents WHERE thread_id = ?1",
            [grandchild],
            |row| row.get(0),
        )
        .expect("grandchild document");
    let document = serde_json::from_str::<serde_json::Value>(&document).expect("document JSON");
    assert_eq!(document["snapshot"]["identity"]["depth"], 2);
}

fn fixture(inject_failure: bool) -> (TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("v27.sqlite3");
    create_v27(&path, inject_failure);
    (directory, path)
}

fn migrate_fixture_to_v28(path: &Path) {
    let mut connection = Connection::open(path).expect("open v27 fixture");
    let transaction = connection.transaction().expect("v28 transaction");
    crate::schema::validate_legacy_source(&transaction).expect("validate v27");
    let converted = crate::legacy::convert(&transaction).expect("convert v27");
    crate::schema::install_v28(&transaction, &converted).expect("install v28");
    crate::schema::validate_v28(&transaction).expect("validate v28");
    transaction.commit().expect("commit v28 fixture");
}

fn migrate_fixture_to_v29(path: &Path) {
    migrate_fixture_to_v28(path);
    let mut connection = Connection::open(path).expect("open v28 fixture");
    let transaction = connection.transaction().expect("v29 transaction");
    crate::schema::install_v29(&transaction).expect("install v29");
    transaction
        .execute(
            "INSERT INTO project_merged_pull_requests (
                project_id, pr, merged_at, detected_at
             ) VALUES ('project-1', 42, ?1, ?1)",
            [STARTED_AT],
        )
        .expect("insert v29 merged row");
    crate::schema::validate_v29(&transaction).expect("validate v29");
    transaction.commit().expect("commit v29 fixture");
}

fn migrate_fixture_to_v30(path: &Path) {
    migrate_fixture_to_v29(path);
    let mut connection = Connection::open(path).expect("open v29 fixture");
    let transaction = connection.transaction().expect("v30 transaction");
    crate::schema::install_v30(&transaction).expect("install v30");
    crate::schema::validate_v30(&transaction).expect("validate v30");
    transaction.commit().expect("commit v30 fixture");
}

fn create_v27(path: &Path, inject_failure: bool) {
    let connection = Connection::open(path).expect("create fixture");
    connection
        .execute_batch(
            "CREATE TABLE settings (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
             INSERT INTO settings VALUES ('toasty_schema_version', '27');
             CREATE TABLE agents (
                id TEXT PRIMARY KEY NOT NULL, runtime_agent_id TEXT NOT NULL,
                parent_id TEXT, role TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL
             );
             CREATE TABLE agent_sessions (
                id TEXT PRIMARY KEY NOT NULL, agent_id TEXT NOT NULL,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                input_tokens BIGINT NOT NULL, cached_input_tokens BIGINT NOT NULL,
                output_tokens BIGINT NOT NULL, reasoning_output_tokens BIGINT NOT NULL,
                total_tokens BIGINT NOT NULL, last_context_tokens BIGINT,
                trace_sequence BIGINT NOT NULL
             );
             CREATE TABLE agent_runtime_states (
                agent_id TEXT PRIMARY KEY NOT NULL, active_session_id TEXT,
                lifecycle TEXT NOT NULL, activity TEXT NOT NULL, active_turn_id TEXT,
                pending_inputs BIGINT NOT NULL, last_turn_json TEXT,
                revision BIGINT NOT NULL, event_sequence BIGINT NOT NULL,
                updated_at BIGINT NOT NULL
             );
             CREATE TABLE agent_pending_inputs (id TEXT PRIMARY KEY NOT NULL);
             CREATE TABLE agent_history_items (
                id TEXT PRIMARY KEY NOT NULL, agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL, position BIGINT NOT NULL, item_json TEXT NOT NULL
             );
             CREATE TABLE agent_messages (
                id TEXT PRIMARY KEY NOT NULL, agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL, position BIGINT NOT NULL, content TEXT NOT NULL
             );
             CREATE TABLE agent_turns (
                turn_id TEXT PRIMARY KEY NOT NULL, agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL, status TEXT NOT NULL, error TEXT,
                started_at BIGINT, finished_at BIGINT
             );
             CREATE TABLE project_review_jobs (
                id TEXT PRIMARY KEY NOT NULL, status TEXT NOT NULL,
                lease_owner TEXT, lease_expires_at TEXT
             );
             CREATE TABLE project_review_runs (
                id TEXT PRIMARY KEY NOT NULL, reviewer_agent_id TEXT, turn_id TEXT,
                status TEXT NOT NULL, started_at TEXT NOT NULL, finished_at TEXT,
                messages_json TEXT NOT NULL, events_json TEXT NOT NULL
             );
             CREATE TABLE agent_log_entries (
                id TEXT PRIMARY KEY NOT NULL, session_id TEXT
             );
             CREATE TABLE tool_trace_records (
                id TEXT PRIMARY KEY NOT NULL, session_id TEXT
             );
             CREATE TABLE agent_runtime_events (
                id TEXT PRIMARY KEY NOT NULL, agent_id TEXT NOT NULL,
                sequence BIGINT NOT NULL, created_at BIGINT NOT NULL, event_json TEXT NOT NULL
             );
             CREATE TABLE agent_runtime_traces (
                id TEXT PRIMARY KEY NOT NULL, agent_id TEXT NOT NULL,
                sequence BIGINT NOT NULL, trace_json TEXT NOT NULL
             );
             CREATE TABLE product_events (
                sequence BIGINT PRIMARY KEY NOT NULL, timestamp TEXT NOT NULL,
                agent_id TEXT, event_json TEXT NOT NULL
             );
             CREATE TABLE session_event_journal (id TEXT PRIMARY KEY NOT NULL);
             CREATE TABLE session_view_snapshots (session_id TEXT PRIMARY KEY NOT NULL);",
        )
        .expect("create schema");
    connection
        .execute(
            "INSERT INTO agents VALUES (?1, ?1, NULL, 'executor', ?2, ?2)",
            params![AGENT_ID, STARTED_AT],
        )
        .expect("agent");
    connection
        .execute(
            "INSERT INTO agent_sessions VALUES (?1, ?2, ?3, ?3, 3, 1, 2, 0, 5, 4, 0)",
            params![SESSION_ID, AGENT_ID, STARTED_AT],
        )
        .expect("session");
    connection
        .execute(
            "INSERT INTO agent_runtime_states VALUES (
                ?1, ?2, 'active', 'idle', NULL, 0, NULL, 1, 1, 1770000000
             )",
            params![AGENT_ID, SESSION_ID],
        )
        .expect("runtime");
    connection
        .execute(
            "INSERT INTO agent_history_items VALUES (
                'context-1', ?1, ?2, 0,
                '{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":{\"text\":\"hello\"},\"metadata\":{}}}'
             )",
            params![AGENT_ID, SESSION_ID],
        )
        .expect("history");
    connection
        .execute(
            "INSERT INTO agent_messages VALUES ('message-1', ?1, ?2, 0, 'hello')",
            params![AGENT_ID, SESSION_ID],
        )
        .expect("message");
    connection
        .execute(
            "INSERT INTO project_review_jobs VALUES ('job-1', 'succeeded', NULL, NULL)",
            [],
        )
        .expect("job");
    connection
        .execute(
            "INSERT INTO project_review_runs VALUES (
                'review-run', ?1, 'review-turn', 'succeeded', ?2, ?2, ?3, '[]'
             )",
            params![
                AGENT_ID,
                STARTED_AT,
                format!(
                    "[{{\"role\":\"assistant\",\"content\":\"review complete\",\"created_at\":\"{STARTED_AT}\"}}]"
                )
            ],
        )
        .expect("review run");
    connection
        .execute(
            "INSERT INTO product_events VALUES (47267, ?1, NULL, ?2)",
            params![STARTED_AT, legacy_product_event_json()],
        )
        .expect("legacy product event");
    if inject_failure {
        connection
            .execute_batch(
                "CREATE TRIGGER fail_review_history
                 BEFORE UPDATE ON project_review_runs
                 BEGIN SELECT RAISE(ABORT, 'injected migration failure'); END;",
            )
            .expect("failure trigger");
    }
}

fn legacy_product_event_json() -> &'static str {
    r#"{"sequence":47267,"timestamp":"2026-08-11T04:08:10Z","type":"agent_updated","agent":{"id":"7f16fb92-8166-45e1-b6b4-29c6a9536a10","parent_id":null,"task_id":null,"project_id":null,"role":"reviewer","name":"reviewer","state":{"resource":"deleted","resource_error":null,"runtime":{"lifecycle":"closing","activity":"idle","active_turn":null,"pending_inputs":0,"last_turn":{"turn_id":"review-turn","session_id":"legacy-session","outcome":"completed","reason":null,"usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0,"total_tokens":2},"finished_at":"2026-08-11T04:08:08Z"},"revision":1}},"container_id":null,"docker_image":"test","provider_id":"openai","provider_name":"OpenAI","model":"test","reasoning_effort":null,"created_at":"2026-08-11T04:00:00Z","updated_at":"2026-08-11T04:08:10Z","token_usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0,"total_tokens":2}}}"#
}
