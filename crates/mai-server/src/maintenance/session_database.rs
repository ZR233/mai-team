//! 仅识别和检查 PL 会话文件；schema 转换始终交给 pl-core。

use anyhow::{Context, Result, ensure};
use pl_core::persistence::SESSION_SCHEMA_VERSION;
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// 一张原始表的行数与类型、长度分帧的逐行摘要。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableDigest {
    pub table: String,
    pub rows: u64,
    pub sha256: String,
}

/// schema 11→当前版本前后必须保持一致的原始数据事实。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInspection {
    pub schema: i64,
    pub original_tables: Vec<TableDigest>,
    pub indexed_tasks: Option<u64>,
    pub indexed_resources: Option<u64>,
}

pub(super) fn discover(data_root: &Path) -> Result<Vec<PathBuf>> {
    let root = data_root.join("sessions");
    let metadata = match root.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "sessions 必须是真实目录"
    );
    let mut paths = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(
            !kind.is_symlink(),
            "会话目录不能包含符号链接：{}",
            entry.path().display()
        );
        if !kind.is_dir() {
            continue;
        }
        let path = entry.path().join("history.sqlite");
        let metadata = match path.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "会话库必须是真实常规文件：{}",
            path.display()
        );
        let id = entry.file_name();
        let id = id.to_str().context("会话目录不是 UTF-8 Agent 身份")?;
        ensure!(
            Uuid::parse_str(id)?.to_string() == id,
            "会话目录必须使用规范 Agent UUID：{id}"
        );
        paths.push(path);
    }
    paths.sort();
    Ok(paths)
}

pub(super) fn inspect(path: &Path) -> Result<SessionInspection> {
    let owner = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|id| id.to_str())
        .context("会话库缺少 Agent 目录")?;
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    ensure!(integrity == "ok", "会话库完整性检查失败：{integrity}");
    let schema = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if schema == 0 {
        let tables: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND substr(name,1,7)<>'sqlite_'",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            tables == 0,
            "schema 0 会话库已含业务表，不能当作未初始化空库"
        );
        // 首次建库前中断留下的空文件由正常 PL open 初始化；离线迁移不写入它。
        return Ok(SessionInspection {
            schema,
            original_tables: Vec::new(),
            indexed_tasks: None,
            indexed_resources: None,
        });
    }
    ensure!(
        schema == 11 || schema == SESSION_SCHEMA_VERSION,
        "会话库 schema {schema} 不在本次支持范围 11→{SESSION_SCHEMA_VERSION} 内"
    );
    let mut original_tables = Vec::new();
    for (table, order) in [
        ("session_entries", "session_id,id"),
        ("session_entry_history", "session_id,sequence"),
        ("session_history_heads", "session_id"),
        ("thread_checkpoints", "thread_id"),
    ] {
        let mut statement =
            connection.prepare(&format!("SELECT * FROM {table} ORDER BY {order}"))?;
        let columns = statement.column_count();
        let mut rows = statement.query([])?;
        let mut hash = Sha256::new();
        let mut count = 0;
        while let Some(row) = rows.next()? {
            ensure!(
                row.get_ref(0)? == ValueRef::Text(owner.as_bytes()),
                "会话数据不属于目录 Agent {owner}：{table}"
            );
            for column in 0..columns {
                match row.get_ref(column)? {
                    ValueRef::Null => hash.update([0]),
                    ValueRef::Integer(value) => {
                        hash.update([1]);
                        hash.update(value.to_le_bytes());
                    }
                    ValueRef::Real(value) => {
                        hash.update([2]);
                        hash.update(value.to_bits().to_le_bytes());
                    }
                    ValueRef::Text(value) | ValueRef::Blob(value) => {
                        hash.update([if matches!(row.get_ref(column)?, ValueRef::Text(_)) {
                            3
                        } else {
                            4
                        }]);
                        hash.update((value.len() as u64).to_le_bytes());
                        hash.update(value);
                    }
                }
            }
            count += 1;
        }
        original_tables.push(TableDigest {
            table: table.into(),
            rows: count,
            sha256: format!("{:x}", hash.finalize()),
        });
    }
    let indexed = |table: &str| -> Result<Option<u64>> {
        if schema == 11 {
            return Ok(None);
        }
        let count: i64 =
            connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?;
        Ok(Some(u64::try_from(count)?))
    };
    Ok(SessionInspection {
        schema,
        original_tables,
        indexed_tasks: indexed("thread_tool_tasks")?,
        indexed_resources: indexed("thread_tool_task_resources")?,
    })
}

pub(super) fn backup(source: &Path, destination: &Path) -> Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let connection = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.backup("main", destination, None)?;
    fs::File::open(destination)?.sync_all()?;
    Ok(())
}
