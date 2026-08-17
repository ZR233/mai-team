use crate::schema::{
    SCHEMA_VERSION, SETTING_SCHEMA_VERSION, build_db, database_schema_version, has_sqlite_header,
};
use crate::settings::set_setting_on;
use crate::*;
use tokio::sync::Mutex;

pub struct MaiStore {
    pub(crate) path: PathBuf,
    pub(crate) config_path: PathBuf,
    pub(crate) artifact_index_dir: PathBuf,
    pub(crate) db: Db,
    pub(crate) git_accounts_lock: Mutex<()>,
}

impl MaiStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_config_path(path, Self::default_config_path()?).await
    }

    pub async fn open_in_data_dir(data_path: impl AsRef<Path>) -> Result<Self> {
        let data_path = data_path.as_ref();
        Self::open_with_config_and_artifact_index_path(
            data_path.join("mai-team.sqlite3"),
            data_path.join("config.toml"),
            data_path.join("artifacts").join("index"),
        )
        .await
    }

    pub async fn open_with_config_path(
        path: impl AsRef<Path>,
        config_path: impl AsRef<Path>,
    ) -> Result<Self> {
        let artifact_index_dir = Self::default_artifact_index_dir()?;
        Self::open_with_config_and_artifact_index_path(path, config_path, artifact_index_dir).await
    }

    pub async fn open_with_config_and_artifact_index_path(
        path: impl AsRef<Path>,
        config_path: impl AsRef<Path>,
        artifact_index_dir: impl AsRef<Path>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let config_path = config_path.as_ref().to_path_buf();
        let artifact_index_dir = artifact_index_dir.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let was_empty = !path.exists() || path.metadata().is_ok_and(|metadata| metadata.len() == 0);
        if !was_empty && !has_sqlite_header(&path)? {
            return Err(StoreError::InvalidConfig(format!(
                "数据库 `{}` 不是有效的 SQLite 文件；为避免数据丢失，mai-server 拒绝覆盖",
                path.display()
            )));
        }

        if !was_empty {
            let version = database_schema_version(&path).map_err(|error| {
                StoreError::InvalidConfig(format!(
                    "无法读取数据库 `{}` 的 schema 版本: {error}",
                    path.display()
                ))
            })?;
            if version.as_deref() != Some(SCHEMA_VERSION) {
                return Err(StoreError::InvalidConfig(format!(
                    "数据库 schema 为 {}，mai-server 仅支持 {SCHEMA_VERSION}；请先停止服务并运行 mai-migrate",
                    version.as_deref().unwrap_or("未标记")
                )));
            }
        }

        let mut db = build_db(&path).await?;
        if was_empty {
            db.push_schema().await?;
            set_setting_on(&mut db, SETTING_SCHEMA_VERSION, SCHEMA_VERSION).await?;
        }

        let store = Self {
            path,
            config_path,
            artifact_index_dir,
            db,
            git_accounts_lock: Mutex::new(()),
        };
        Ok(store)
    }

    pub fn default_path() -> Result<PathBuf> {
        Ok(Self::default_data_dir()?.join("mai-team.sqlite3"))
    }

    pub fn default_config_path() -> Result<PathBuf> {
        Ok(Self::default_data_dir()?.join("config.toml"))
    }

    pub fn default_artifact_index_dir() -> Result<PathBuf> {
        Ok(Self::default_data_dir()?.join("artifacts").join("index"))
    }

    pub fn default_data_dir() -> Result<PathBuf> {
        Ok(std::env::current_dir()?.join(".mai-team"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// 返回只负责 serde 配置文档 IO 的独立存储。
    pub fn config_documents(&self) -> crate::ConfigDocumentStore {
        crate::ConfigDocumentStore::new(self.config_path.clone())
    }

    pub fn artifact_index_dir(&self) -> &Path {
        &self.artifact_index_dir
    }
}
