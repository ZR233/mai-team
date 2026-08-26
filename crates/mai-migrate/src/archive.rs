use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::Connection;
use rusqlite::backup::Backup;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{MigrationOptions, SOURCE_SCHEMA, TARGET_SCHEMA};

const DATABASE_FILE: &str = "mai-team-schema31.sqlite3";
const MANIFEST_FILE: &str = "manifest.json";

/// 不随在线数据库清理的 PL v1 完整归档清单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveManifest {
    pub archive_id: String,
    pub created_at: String,
    pub source_database: String,
    pub archived_database: String,
    pub database_sha256: String,
    pub source_schema: String,
    pub target_schema: String,
    pub source_commit: String,
    pub target_commit: String,
    pub row_counts: BTreeMap<String, usize>,
}

pub(crate) fn create(
    source_path: &Path,
    source: &Connection,
    options: &MigrationOptions,
) -> Result<ArchiveManifest> {
    let created_at = Utc::now();
    let archive_id = format!("pl-v2-{}", created_at.format("%Y%m%dT%H%M%SZ"));
    let directory = options.archive_root.join(&archive_id);
    fs::create_dir_all(&options.archive_root)
        .with_context(|| format!("无法创建框架归档根目录 {}", options.archive_root.display()))?;
    if directory.exists() {
        bail!("归档目录已存在，拒绝覆盖: {}", directory.display());
    }
    fs::create_dir(&directory)
        .with_context(|| format!("无法创建归档目录 {}", directory.display()))?;
    protect_directory(&directory)?;

    let database_path = directory.join(DATABASE_FILE);
    let mut destination = Connection::open(&database_path)
        .with_context(|| format!("无法创建归档数据库 {}", database_path.display()))?;
    {
        let backup = Backup::new(source, &mut destination)?;
        backup.run_to_completion(256, Duration::from_millis(1), None)?;
    }
    drop(destination);

    let archived = Connection::open(&database_path)?;
    let integrity: String = archived.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("归档数据库完整性校验失败: {integrity}");
    }
    let archived_schema = crate::schema::schema_version(&archived)?;
    if archived_schema != SOURCE_SCHEMA {
        bail!("归档数据库 schema 应为 {SOURCE_SCHEMA}，实际为 {archived_schema}");
    }
    let source_counts = row_counts(source)?;
    let archived_counts = row_counts(&archived)?;
    if archived_counts != source_counts {
        bail!("归档数据库行数与源数据库不一致");
    }
    drop(archived);

    let database_sha256 = sha256(&database_path)?;
    let manifest = ArchiveManifest {
        archive_id,
        created_at: created_at.to_rfc3339(),
        source_database: source_path.display().to_string(),
        archived_database: database_path.display().to_string(),
        database_sha256,
        source_schema: SOURCE_SCHEMA.to_string(),
        target_schema: TARGET_SCHEMA.to_string(),
        source_commit: required_commit("source_commit", &options.source_commit)?,
        target_commit: required_commit("target_commit", &options.target_commit)?,
        row_counts: source_counts,
    };
    write_manifest(&directory, &manifest)?;
    Ok(manifest)
}

fn required_commit(name: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("归档清单缺少 {name}");
    }
    Ok(value.to_string())
}

fn row_counts(connection: &Connection) -> Result<BTreeMap<String, usize>> {
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name",
    )?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut counts = BTreeMap::new();
    for table in tables {
        let quoted = table.replace('"', "\"\"");
        let count: i64 =
            connection.query_row(&format!("SELECT COUNT(*) FROM \"{quoted}\""), [], |row| {
                row.get(0)
            })?;
        counts.insert(table, usize::try_from(count)?);
    }
    Ok(counts)
}

fn sha256(path: &Path) -> Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn write_manifest(directory: &Path, manifest: &ArchiveManifest) -> Result<()> {
    let final_path = directory.join(MANIFEST_FILE);
    let temporary_path = directory.join("manifest.json.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)?;
    file.write_all(&serde_json::to_vec_pretty(manifest)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary_path, &final_path)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn protect_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn protect_directory(_path: &Path) -> Result<()> {
    Ok(())
}
