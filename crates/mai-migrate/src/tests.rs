use std::fs;
use std::path::{Path, PathBuf};

use pretty_assertions::assert_eq;
use rusqlite::{Connection, OptionalExtension};
use tempfile::TempDir;

use crate::{MigrationOptions, migrate_path, validate_path};

const AGENT_ID: &str = "7f16fb92-8166-45e1-b6b4-29c6a9536a10";
const PROJECT_ID: &str = "21eeef66-91d0-4dad-a3ed-8ada7e84d8f9";
const ARCHIVED_HISTORY: &str = r#"{"schemaVersion":31,"items":[{"legacy":true}]}"#;

#[test]
fn schema31_to_32_archives_full_database_and_rebuilds_only_framework_state() {
    let fixture = Fixture::new();
    let before = product_facts(&fixture.path);
    let report = migrate_path(&fixture.path, &fixture.options()).expect("migrate schema 31");

    assert_eq!(report.source_schema, "31");
    assert_eq!(report.target_schema, "32");
    assert!(!report.already_current);
    assert_eq!(report.agents, 1);
    assert_eq!(report.product_review_jobs, 1);
    assert_eq!(report.product_review_runs, 1);
    assert_eq!(report.archived_review_runs, 1);
    assert_eq!(report.reset_threads, 1);
    assert_eq!(report.reset_turns, 1);
    assert_eq!(report.reset_items, 1);

    let manifest = report.archive.expect("archive manifest");
    assert_eq!(manifest.source_commit, "source-commit");
    assert_eq!(manifest.target_commit, "target-commit");
    assert_eq!(manifest.source_schema, "31");
    assert_eq!(manifest.target_schema, "32");
    assert_eq!(manifest.row_counts["agents"], 1);
    assert_eq!(manifest.row_counts["thread_runtime_documents"], 1);
    assert_eq!(manifest.database_sha256.len(), 64);

    let archive_database = PathBuf::from(&manifest.archived_database);
    let archive_directory = archive_database.parent().expect("archive directory");
    assert!(archive_database.is_file());
    assert!(archive_directory.join("manifest.json").is_file());
    let archived = Connection::open(&archive_database).expect("open archived database");
    assert_eq!(schema_version(&archived), "31");
    assert_eq!(count(&archived, "thread_runtime_documents"), 1);
    assert_eq!(
        review_history(&archived),
        Some(ARCHIVED_HISTORY.to_string())
    );

    let migrated = Connection::open(&fixture.path).expect("open migrated database");
    assert_eq!(schema_version(&migrated), "32");
    assert_eq!(count(&migrated, "thread_runtime_documents"), 0);
    assert_eq!(count(&migrated, "thread_turns"), 0);
    assert_eq!(count(&migrated, "thread_items"), 0);
    assert_eq!(count(&migrated, "product_events"), 0);
    assert_eq!(product_facts(&fixture.path), before);
    let timeline = migrated
        .query_row(
            "SELECT history_json, history_status, history_archive_id, history_archived_at
             FROM project_review_runs WHERE id = 'run-1'",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .expect("timeline marker");
    assert_eq!(timeline.0, None);
    assert_eq!(timeline.1, "pl_v2_archived");
    assert_eq!(timeline.2, Some(manifest.archive_id));
    assert!(timeline.3.is_some());
    let skills_config: String = migrated
        .query_row(
            "SELECT value FROM settings WHERE key = 'skills_config'",
            [],
            |row| row.get(0),
        )
        .expect("migrated skills config");
    assert_eq!(skills_config, r#"{"disabled":["review"]}"#);

    let repeated = migrate_path(&fixture.path, &fixture.options()).expect("validate current");
    assert!(repeated.already_current);
    assert_eq!(repeated.archive, None);
    assert_eq!(
        validate_path(&fixture.path).expect("validate target"),
        repeated
    );
}

#[test]
fn nonterminal_review_or_lease_rejects_migration_before_archive_creation() {
    let fixture = Fixture::new();
    let connection = Connection::open(&fixture.path).expect("open fixture");
    connection
        .execute(
            "UPDATE project_review_jobs
             SET status = 'running', lease_owner = 'server-1', lease_expires_at = '2099-01-01Z'",
            [],
        )
        .expect("activate job");
    drop(connection);

    let error = migrate_path(&fixture.path, &fixture.options()).expect_err("must reject live job");
    assert!(error.to_string().contains("非终态 Review Job"));
    assert!(!fixture.archive_root.exists());
    assert_eq!(
        schema_version(&Connection::open(&fixture.path).unwrap()),
        "31"
    );
}

#[test]
fn terminal_review_job_stale_lease_is_cleared_by_schema32_transaction() {
    let fixture = Fixture::new();
    let connection = Connection::open(&fixture.path).expect("open fixture");
    connection
        .execute(
            "UPDATE project_review_jobs
             SET status = 'superseded',
                 lease_owner = 'retired-server',
                 lease_expires_at = '2026-08-26T00:00:00Z'",
            [],
        )
        .expect("seed terminal stale lease");
    drop(connection);

    migrate_path(&fixture.path, &fixture.options()).expect("migrate terminal stale lease");

    let connection = Connection::open(&fixture.path).expect("inspect migrated fixture");
    let lease = connection
        .query_row(
            "SELECT lease_owner, lease_expires_at FROM project_review_jobs WHERE id = 'job-1'",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .expect("read migrated lease");
    assert_eq!(lease, (None, None));
}

#[test]
fn backup_failure_aborts_without_touching_the_online_database() {
    let fixture = Fixture::new();
    fs::write(&fixture.archive_root, b"not a directory").expect("block archive root");

    let error = migrate_path(&fixture.path, &fixture.options()).expect_err("backup must fail");
    assert!(format!("{error:#}").contains("归档根目录"));
    let connection = Connection::open(&fixture.path).expect("open untouched fixture");
    assert_eq!(schema_version(&connection), "31");
    assert_eq!(count(&connection, "thread_runtime_documents"), 1);
    assert_eq!(
        review_history(&connection),
        Some(ARCHIVED_HISTORY.to_string())
    );
}

#[test]
fn migration_write_failure_rolls_back_online_changes_but_preserves_archive() {
    let fixture = Fixture::new();
    let connection = Connection::open(&fixture.path).expect("open fixture");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_schema32
             BEFORE UPDATE OF value ON settings
             WHEN NEW.value = '32'
             BEGIN SELECT RAISE(ABORT, 'injected schema32 failure'); END;",
        )
        .expect("inject failure");
    drop(connection);

    let error = migrate_path(&fixture.path, &fixture.options()).expect_err("migration must fail");
    assert!(format!("{error:#}").contains("injected schema32 failure"));
    let connection = Connection::open(&fixture.path).expect("inspect rollback");
    assert_eq!(schema_version(&connection), "31");
    assert_eq!(count(&connection, "thread_runtime_documents"), 1);
    assert_eq!(
        review_history(&connection),
        Some(ARCHIVED_HISTORY.to_string())
    );
    assert_eq!(
        column_exists(&connection, "project_review_runs", "history_status"),
        false
    );

    let archives = fs::read_dir(&fixture.archive_root)
        .expect("archive root")
        .collect::<Result<Vec<_>, _>>()
        .expect("archive entries");
    assert_eq!(archives.len(), 1);
    assert!(archives[0].path().join("manifest.json").is_file());
}

#[test]
fn unsupported_schema_is_rejected_without_legacy_conversion() {
    let fixture = Fixture::new();
    let connection = Connection::open(&fixture.path).expect("open fixture");
    connection
        .execute(
            "UPDATE settings SET value = '30' WHERE key = 'toasty_schema_version'",
            [],
        )
        .expect("downgrade marker");
    drop(connection);

    let error = migrate_path(&fixture.path, &fixture.options()).expect_err("reject schema 30");
    assert!(error.to_string().contains("只支持从 schema 31"));
    assert!(!fixture.archive_root.exists());
}

struct Fixture {
    _directory: TempDir,
    path: PathBuf,
    archive_root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("mai.sqlite3");
        let archive_root = directory.path().join("framework-archives");
        create_schema31(&path);
        Self {
            _directory: directory,
            path,
            archive_root,
        }
    }

    fn options(&self) -> MigrationOptions {
        MigrationOptions {
            archive_root: self.archive_root.clone(),
            source_commit: "source-commit".to_string(),
            target_commit: "target-commit".to_string(),
        }
    }
}

fn create_schema31(path: &Path) {
    let connection = Connection::open(path).expect("create fixture");
    connection
        .execute_batch(&format!(
            "CREATE TABLE settings (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
             INSERT INTO settings VALUES ('toasty_schema_version', '31');
             INSERT INTO settings VALUES ('skills_config', '{{\"config\":[{{\"name\":\"review\",\"path\":null,\"enabled\":false}}]}}');
             CREATE TABLE agents (
                id TEXT PRIMARY KEY NOT NULL,
                resource_state TEXT NOT NULL,
                resource_error TEXT,
                role TEXT
             );
             INSERT INTO agents VALUES ('{AGENT_ID}', 'ready', NULL, 'maintainer');
             CREATE TABLE projects (
                id TEXT PRIMARY KEY NOT NULL,
                current_reviewer_agent_id TEXT,
                auto_review_enabled BOOLEAN NOT NULL
             );
             INSERT INTO projects VALUES ('{PROJECT_ID}', NULL, 1);
             CREATE TABLE project_review_jobs (
                id TEXT PRIMARY KEY NOT NULL,
                project_id TEXT NOT NULL,
                status TEXT NOT NULL,
                target_head_sha TEXT NOT NULL,
                submission_intent_json TEXT,
                receipt_json TEXT,
                lease_owner TEXT,
                lease_expires_at TEXT
             );
             INSERT INTO project_review_jobs VALUES (
                'job-1', '{PROJECT_ID}', 'succeeded', 'head-sha',
                '{{\"event\":\"comment\"}}', '{{\"reviewId\":42}}', NULL, NULL
             );
             CREATE TABLE project_review_runs (
                id TEXT PRIMARY KEY NOT NULL,
                job_id TEXT NOT NULL,
                status TEXT NOT NULL,
                outcome TEXT,
                summary TEXT,
                history_json TEXT,
                started_at TEXT NOT NULL,
                finished_at TEXT
             );
             INSERT INTO project_review_runs VALUES (
                'run-1', 'job-1', 'succeeded', 'review_submitted', 'done',
                '{}', '2026-08-26T00:00:00Z', '2026-08-26T00:01:00Z'
             );
             CREATE TABLE product_events (
                sequence BIGINT PRIMARY KEY NOT NULL,
                timestamp TEXT NOT NULL,
                agent_id TEXT,
                event_json TEXT NOT NULL
             );
             INSERT INTO product_events VALUES (1, '2026-08-26T00:00:00Z', NULL, '{{}}');
             CREATE TABLE thread_runtime_documents (
                thread_id TEXT PRIMARY KEY NOT NULL,
                revision BIGINT NOT NULL,
                document_json TEXT NOT NULL,
                snapshot_json TEXT,
                updated_at BIGINT NOT NULL
             );
             INSERT INTO thread_runtime_documents VALUES (
                '{AGENT_ID}', 1,
                '{{\"snapshot\":{{\"lifecycle\":\"active\",\"activity\":\"idle\",\"activeTurnId\":null,\"pendingInputs\":0}}}}',
                NULL, 1
             );
             CREATE TABLE thread_turns (id TEXT PRIMARY KEY NOT NULL);
             INSERT INTO thread_turns VALUES ('turn-1');
             CREATE TABLE thread_items (id TEXT PRIMARY KEY NOT NULL);
             INSERT INTO thread_items VALUES ('item-1');
             CREATE TABLE thread_runtime_events (id TEXT PRIMARY KEY NOT NULL);
             INSERT INTO thread_runtime_events VALUES ('event-1');
             CREATE TABLE thread_runtime_traces (id TEXT PRIMARY KEY NOT NULL);
             INSERT INTO thread_runtime_traces VALUES ('trace-1');
             CREATE TABLE thread_notifications (id TEXT PRIMARY KEY NOT NULL);
             INSERT INTO thread_notifications VALUES ('notification-1');
             CREATE TABLE thread_submissions (id TEXT PRIMARY KEY NOT NULL);
             INSERT INTO thread_submissions VALUES ('submission-1');",
            ARCHIVED_HISTORY.replace('\'', "''")
        ))
        .expect("create schema 31 fixture");
}

fn product_facts(path: &Path) -> (String, String, String, String, String, String) {
    let connection = Connection::open(path).expect("open product facts");
    connection
        .query_row(
            "SELECT jobs.id, jobs.target_head_sha, jobs.submission_intent_json,
                    jobs.receipt_json, runs.outcome, runs.summary
             FROM project_review_jobs jobs
             JOIN project_review_runs runs ON runs.job_id = jobs.id",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .expect("product facts")
}

fn schema_version(connection: &Connection) -> String {
    connection
        .query_row(
            "SELECT value FROM settings WHERE key = 'toasty_schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("schema version")
}

fn count(connection: &Connection, table: &str) -> usize {
    let value: i64 = connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("row count");
    usize::try_from(value).expect("nonnegative count")
}

fn review_history(connection: &Connection) -> Option<String> {
    connection
        .query_row(
            "SELECT history_json FROM project_review_runs WHERE id = 'run-1'",
            [],
            |row| row.get(0),
        )
        .expect("review history")
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> bool {
    connection
        .query_row(
            &format!("SELECT name FROM pragma_table_info('{table}') WHERE name = ?1"),
            [column],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .expect("column lookup")
        .is_some()
}
