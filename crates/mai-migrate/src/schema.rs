use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use mai_protocol::MaiProductEventEnvelope;
use pl_protocol::ThreadTurnHistory;
use rusqlite::{OptionalExtension, Transaction, params};

use crate::legacy::ConvertedData;
use crate::{MigrationReport, SOURCE_SCHEMA, TARGET_SCHEMA};

const VERSION_KEY: &str = "toasty_schema_version";

pub(crate) fn schema_version(transaction: &Transaction<'_>) -> Result<String> {
    transaction
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![VERSION_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .context("数据库缺少 toasty_schema_version")
}

pub(crate) fn validate_source(transaction: &Transaction<'_>) -> Result<()> {
    if schema_version(transaction)? != SOURCE_SCHEMA {
        bail!("源数据库不是 schema {SOURCE_SCHEMA}");
    }
    for (table, columns) in [
        ("agents", &["id", "runtime_agent_id"][..]),
        (
            "agent_sessions",
            &["id", "agent_id", "updated_at", "trace_sequence"][..],
        ),
        (
            "agent_runtime_states",
            &["agent_id", "active_session_id", "revision"][..],
        ),
        (
            "agent_history_items",
            &["id", "agent_id", "session_id", "item_json"][..],
        ),
        (
            "agent_messages",
            &["id", "agent_id", "session_id", "content"][..],
        ),
        (
            "agent_turns",
            &["turn_id", "agent_id", "session_id", "status"][..],
        ),
        (
            "project_review_runs",
            &["id", "messages_json", "events_json"][..],
        ),
        ("agent_log_entries", &["id", "session_id"][..]),
        ("tool_trace_records", &["id", "session_id"][..]),
        (
            "product_events",
            &["sequence", "timestamp", "agent_id", "event_json"][..],
        ),
    ] {
        require_columns(transaction, table, columns)?;
    }
    for table in [
        "thread_runtime_documents",
        "thread_turns",
        "thread_items",
        "thread_notifications",
    ] {
        if table_exists(transaction, table)? {
            bail!("v27 数据库意外包含目标表 `{table}`");
        }
    }
    Ok(())
}

pub(crate) fn install_target(
    transaction: &Transaction<'_>,
    converted: &ConvertedData,
) -> Result<()> {
    transaction.execute_batch(
        "CREATE TABLE thread_runtime_documents (
            thread_id TEXT NOT NULL PRIMARY KEY,
            revision BIGINT NOT NULL,
            document_json TEXT NOT NULL,
            snapshot_json TEXT,
            updated_at BIGINT NOT NULL
         );
         CREATE TABLE thread_turns (
            id TEXT NOT NULL PRIMARY KEY,
            thread_id TEXT NOT NULL,
            ordinal BIGINT NOT NULL,
            turn_json TEXT NOT NULL,
            model_json TEXT,
            context_disposition TEXT NOT NULL
         );
         CREATE INDEX index_thread_turns_by_thread_id ON thread_turns(thread_id);
         CREATE TABLE thread_items (
            id TEXT NOT NULL PRIMARY KEY,
            thread_id TEXT NOT NULL,
            turn_id TEXT NOT NULL,
            ordinal BIGINT NOT NULL,
            revision BIGINT NOT NULL,
            item_json TEXT NOT NULL
         );
         CREATE INDEX index_thread_items_by_thread_id ON thread_items(thread_id);
         CREATE INDEX index_thread_items_by_turn_id ON thread_items(turn_id);
         CREATE TABLE thread_notifications (
            id TEXT NOT NULL PRIMARY KEY,
            thread_id TEXT NOT NULL,
            revision BIGINT NOT NULL,
            emitted_at BIGINT NOT NULL,
            notification_json TEXT NOT NULL
         );
         CREATE INDEX index_thread_notifications_by_thread_id
            ON thread_notifications(thread_id);",
    )?;

    for runtime in &converted.runtimes {
        transaction.execute(
            "INSERT INTO thread_runtime_documents (
                thread_id, revision, document_json, snapshot_json, updated_at
             ) VALUES (?1, ?2, ?3, NULL, ?4)",
            params![
                runtime.thread_id,
                i64::try_from(runtime.revision)?,
                runtime.document_json,
                runtime.updated_at
            ],
        )?;
    }

    let mut histories = converted.histories.iter().collect::<Vec<_>>();
    histories.sort_by(|left, right| {
        left.turn
            .thread_id
            .cmp(&right.turn.thread_id)
            .then_with(|| left.turn.updated_at.cmp(&right.turn.updated_at))
            .then_with(|| left.turn.id.cmp(&right.turn.id))
    });
    let mut previous_thread = None::<&str>;
    let mut ordinal = 0_i64;
    for history in histories {
        if previous_thread != Some(history.turn.thread_id.as_str()) {
            previous_thread = Some(history.turn.thread_id.as_str());
            ordinal = 0;
        }
        transaction.execute(
            "INSERT INTO thread_turns (
                id, thread_id, ordinal, turn_json, model_json, context_disposition
             ) VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            params![
                history.turn.id,
                history.turn.thread_id,
                ordinal,
                serde_json::to_string(&history.turn)?,
                serde_json::to_string(&history.context_disposition)?,
            ],
        )?;
        ordinal = ordinal.checked_add(1).context("Thread turn ordinal 溢出")?;
        for item in &history.items {
            transaction.execute(
                "INSERT INTO thread_items (
                    id, thread_id, turn_id, ordinal, revision, item_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    item.id,
                    item.thread_id,
                    item.turn_id,
                    i64::try_from(item.ordinal)?,
                    i64::try_from(item.revision)?,
                    serde_json::to_string(item)?,
                ],
            )?;
        }
    }

    transaction.execute(
        "ALTER TABLE project_review_runs ADD COLUMN history_json TEXT",
        [],
    )?;
    for (run_id, history) in &converted.review_histories {
        transaction.execute(
            "UPDATE project_review_runs SET history_json = ?1 WHERE id = ?2",
            params![serde_json::to_string(history)?, run_id],
        )?;
    }
    let missing_history: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM project_review_runs WHERE history_json IS NULL",
        [],
        |row| row.get(0),
    )?;
    if missing_history != 0 {
        bail!("有 {missing_history} 条 review run 未生成归档 history");
    }

    transaction.execute_batch(
        "ALTER TABLE project_review_runs DROP COLUMN messages_json;
         ALTER TABLE project_review_runs DROP COLUMN events_json;
         ALTER TABLE agents DROP COLUMN runtime_agent_id;
         ALTER TABLE agent_log_entries RENAME COLUMN session_id TO thread_id;
         ALTER TABLE tool_trace_records RENAME COLUMN session_id TO thread_id;
         ALTER TABLE agent_runtime_events RENAME TO thread_runtime_events;
         ALTER TABLE thread_runtime_events RENAME COLUMN agent_id TO thread_id;
         ALTER TABLE agent_runtime_traces RENAME TO thread_runtime_traces;
         ALTER TABLE thread_runtime_traces RENAME COLUMN agent_id TO thread_id;
         DROP TABLE agent_pending_inputs;
         DROP TABLE agent_turns;
         DROP TABLE agent_messages;
         DROP TABLE agent_history_items;
         DROP TABLE session_event_journal;
         DROP TABLE session_view_snapshots;
         DROP TABLE agent_sessions;
         DROP TABLE agent_runtime_states;
         DELETE FROM product_events;
         UPDATE settings SET value = '28' WHERE key = 'toasty_schema_version';",
    )?;
    Ok(())
}

pub(crate) fn validate_target(
    transaction: &Transaction<'_>,
    already_current: bool,
) -> Result<MigrationReport> {
    if schema_version(transaction)? != TARGET_SCHEMA {
        bail!("目标数据库不是 schema {TARGET_SCHEMA}");
    }
    for (table, columns) in [
        (
            "thread_runtime_documents",
            &["thread_id", "revision", "document_json", "snapshot_json"][..],
        ),
        (
            "thread_turns",
            &[
                "id",
                "thread_id",
                "turn_json",
                "model_json",
                "context_disposition",
            ][..],
        ),
        (
            "thread_items",
            &["id", "thread_id", "turn_id", "revision", "item_json"][..],
        ),
        (
            "thread_notifications",
            &["id", "thread_id", "revision", "notification_json"][..],
        ),
        (
            "thread_runtime_events",
            &["id", "thread_id", "sequence", "created_at", "event_json"][..],
        ),
        (
            "thread_runtime_traces",
            &["id", "thread_id", "sequence", "trace_json"][..],
        ),
        (
            "product_events",
            &["sequence", "timestamp", "agent_id", "event_json"][..],
        ),
        ("project_review_runs", &["id", "history_json"][..]),
        ("agent_log_entries", &["id", "thread_id"][..]),
        ("tool_trace_records", &["id", "thread_id"][..]),
    ] {
        require_columns(transaction, table, columns)?;
    }
    for table in [
        "agent_sessions",
        "agent_messages",
        "agent_history_items",
        "agent_pending_inputs",
        "agent_turns",
        "agent_runtime_states",
        "session_event_journal",
        "session_view_snapshots",
    ] {
        if table_exists(transaction, table)? {
            bail!("目标数据库仍包含旧运行表 `{table}`");
        }
    }
    if column_exists(transaction, "agents", "runtime_agent_id")? {
        bail!("目标 agents 仍包含 runtime_agent_id");
    }
    if column_exists(transaction, "project_review_runs", "messages_json")?
        || column_exists(transaction, "project_review_runs", "events_json")?
    {
        bail!("目标 review run 仍包含旧消息投影列");
    }

    let agents = count(transaction, "agents")?;
    let runtimes = count(transaction, "thread_runtime_documents")?;
    if agents != runtimes {
        bail!("Agent 数 {agents} 与 canonical Thread 数 {runtimes} 不一致");
    }
    validate_runtime_documents(transaction)?;
    validate_thread_history(transaction)?;
    validate_product_events(transaction)?;
    let archived_review_runs = validate_review_history(transaction)?;
    Ok(MigrationReport {
        source_schema: if already_current { "28" } else { "27" }.to_string(),
        target_schema: "28".to_string(),
        already_current,
        agents,
        canonical_threads: runtimes,
        turns: count(transaction, "thread_turns")?,
        items: count(transaction, "thread_items")?,
        archived_review_runs,
    })
}

fn validate_product_events(transaction: &Transaction<'_>) -> Result<()> {
    let mut statement = transaction
        .prepare("SELECT sequence, event_json FROM product_events ORDER BY sequence ASC")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })? {
        let (sequence, raw) = row?;
        let event = serde_json::from_str::<MaiProductEventEnvelope>(&raw)
            .with_context(|| format!("product event {sequence} 不符合当前协议"))?;
        if event.sequence != u64::try_from(sequence)? {
            bail!(
                "product event row sequence {sequence} 与 JSON sequence {} 不一致",
                event.sequence
            );
        }
    }
    Ok(())
}

fn validate_runtime_documents(transaction: &Transaction<'_>) -> Result<()> {
    let mut statement = transaction
        .prepare("SELECT thread_id, revision, document_json FROM thread_runtime_documents")?;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (thread_id, revision, raw) = row?;
        if revision < 0 {
            bail!("Thread {thread_id} revision 为负数");
        }
        let document = serde_json::from_str::<serde_json::Value>(&raw)?;
        let identity = document
            .pointer("/snapshot/identity/id")
            .and_then(serde_json::Value::as_str)
            .context("canonical document 缺少 snapshot.identity.id")?;
        if identity != thread_id {
            bail!("canonical document identity {identity} 与 key {thread_id} 不一致");
        }
        let document_revision = document
            .pointer("/snapshot/revision")
            .and_then(serde_json::Value::as_u64)
            .context("canonical document 缺少 snapshot.revision")?;
        if document_revision != u64::try_from(revision)? {
            bail!("Thread {thread_id} revision 不一致");
        }
    }
    Ok(())
}

fn validate_thread_history(transaction: &Transaction<'_>) -> Result<()> {
    let orphan_turns: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM thread_turns turns
         WHERE NOT EXISTS (
            SELECT 1 FROM thread_runtime_documents runtime
            WHERE runtime.thread_id = turns.thread_id
         )",
        [],
        |row| row.get(0),
    )?;
    let orphan_items: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM thread_items items
         WHERE NOT EXISTS (
            SELECT 1 FROM thread_turns turns
            WHERE turns.id = items.turn_id AND turns.thread_id = items.thread_id
         )",
        [],
        |row| row.get(0),
    )?;
    if orphan_turns != 0 || orphan_items != 0 {
        bail!("Thread history 存在孤儿: turns={orphan_turns}, items={orphan_items}");
    }
    let mut statement =
        transaction.prepare("SELECT id, turn_json, model_json FROM thread_turns")?;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })? {
        let (id, raw, model) = row?;
        let turn = serde_json::from_str::<pl_protocol::Turn>(&raw)?;
        if turn.id != id {
            bail!("Turn JSON id {} 与 row id {id} 不一致", turn.id);
        }
        if let Some(model) = model {
            serde_json::from_str::<pl_protocol::TurnBillingRecord>(&model)
                .with_context(|| format!("Turn {id} model_json 非法"))?;
        }
    }
    Ok(())
}

fn validate_review_history(transaction: &Transaction<'_>) -> Result<usize> {
    let mut statement = transaction.prepare("SELECT id, history_json FROM project_review_runs")?;
    let mut count = 0_usize;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })? {
        let (id, raw) = row?;
        let raw = raw.with_context(|| format!("review run {id} 缺少 history"))?;
        let history = serde_json::from_str::<ThreadTurnHistory>(&raw)
            .with_context(|| format!("review run {id} history 非法"))?;
        for item in &history.items {
            if item.thread_id != history.turn.thread_id || item.turn_id != history.turn.id {
                bail!("review run {id} 的 Item {} 归属不一致", item.id);
            }
        }
        count += 1;
    }
    Ok(count)
}

fn require_columns(transaction: &Transaction<'_>, table: &str, required: &[&str]) -> Result<()> {
    if !table_exists(transaction, table)? {
        bail!("数据库缺少表 `{table}`");
    }
    let columns = table_columns(transaction, table)?;
    for column in required {
        if !columns.contains(*column) {
            bail!("表 `{table}` 缺少列 `{column}`");
        }
    }
    Ok(())
}

fn table_exists(transaction: &Transaction<'_>, table: &str) -> Result<bool> {
    Ok(transaction
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn column_exists(transaction: &Transaction<'_>, table: &str, column: &str) -> Result<bool> {
    Ok(table_columns(transaction, table)?.contains(column))
}

fn table_columns(transaction: &Transaction<'_>, table: &str) -> Result<BTreeSet<String>> {
    let mut statement = transaction.prepare(&format!("PRAGMA table_info({table})"))?;
    statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()
        .map_err(Into::into)
}

fn count(transaction: &Transaction<'_>, table: &str) -> Result<usize> {
    let value = transaction.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get::<_, i64>(0)
    })?;
    Ok(usize::try_from(value)?)
}
