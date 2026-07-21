use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::AsyncWriteExt;

use crate::{Result, StoreError};

/// 泛型 serde TOML 配置文档存储。
///
/// 该类型只负责文件 IO，不定义产品 schema、默认值或任何 PL 类型。临时文件与目标文件
/// 位于同一目录，写入并 `sync_all` 后通过 rename 替换，避免暴露半写入文档。
#[derive(Debug, Clone)]
pub struct ConfigDocumentStore {
    path: PathBuf,
}

impl ConfigDocumentStore {
    /// 为明确的配置文件路径创建存储。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 返回配置文档路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取并反序列化配置；文件不存在时返回 `None`。
    pub async fn load<T>(&self) -> Result<Option<T>>
    where
        T: DeserializeOwned,
    {
        let content = match tokio::fs::read_to_string(&self.path).await {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        toml::from_str(&content).map(Some).map_err(StoreError::from)
    }

    /// 序列化配置并使用同目录临时文件原子替换目标文档。
    pub async fn save<T>(&self, document: &T) -> Result<()>
    where
        T: Serialize,
    {
        let content = toml::to_string_pretty(document)?;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let temporary_path = temporary_path(&self.path);
        let result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary_path)
                .await?;
            file.write_all(content.as_bytes()).await?;
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            replace_file(&temporary_path, &self.path).await
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temporary_path).await;
        }
        result
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        timestamp + sequence as u128
    ))
}

#[cfg(unix)]
async fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    tokio::fs::rename(source, destination).await?;
    Ok(())
}

#[cfg(windows)]
async fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    // Windows 标准 rename 不替换已有文件；先保存旧文档，确保任一步失败均可恢复。
    let backup = destination.with_extension("replace-backup");
    if tokio::fs::try_exists(destination).await? {
        let _ = tokio::fs::remove_file(&backup).await;
        tokio::fs::rename(destination, &backup).await?;
    }
    match tokio::fs::rename(source, destination).await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(backup).await;
            Ok(())
        }
        Err(error) => {
            if tokio::fs::try_exists(&backup).await.unwrap_or(false) {
                let _ = tokio::fs::rename(&backup, destination).await;
            }
            Err(error.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct TestDocument {
        schema_version: u32,
        name: String,
    }

    #[tokio::test]
    async fn document_round_trip_and_replacement_are_canonical() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigDocumentStore::new(directory.path().join("config.toml"));
        assert_eq!(store.load::<TestDocument>().await.unwrap(), None);

        store
            .save(&TestDocument {
                schema_version: 1,
                name: "first".to_string(),
            })
            .await
            .unwrap();
        store
            .save(&TestDocument {
                schema_version: 2,
                name: "second".to_string(),
            })
            .await
            .unwrap();

        assert_eq!(
            store.load::<TestDocument>().await.unwrap(),
            Some(TestDocument {
                schema_version: 2,
                name: "second".to_string(),
            })
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
