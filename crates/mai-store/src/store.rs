use crate::providers::ProvidersCache;
use crate::schema::{SCHEMA_VERSION, SETTING_SCHEMA_VERSION, build_db, has_sqlite_header};
use crate::settings::{get_setting_on, set_setting_on};
use crate::*;
use tokio::sync::Mutex;

pub struct ConfigStore {
    pub(crate) path: PathBuf,
    pub(crate) config_path: PathBuf,
    pub(crate) artifact_index_dir: PathBuf,
    pub(crate) db: Db,
    pub(crate) git_accounts_lock: Mutex<()>,
    pub(crate) providers_cache: Mutex<Option<ProvidersCache>>,
}

impl ConfigStore {
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

        let mut was_empty =
            !path.exists() || path.metadata().is_ok_and(|metadata| metadata.len() == 0);
        if !was_empty && !has_sqlite_header(&path)? {
            let _ = std::fs::remove_file(&path);
            was_empty = true;
        }

        let mut db = build_db(&path).await?;
        if was_empty {
            db.push_schema().await?;
            set_setting_on(&mut db, SETTING_SCHEMA_VERSION, SCHEMA_VERSION).await?;
        } else {
            let current_schema_version = get_setting_on(&db, SETTING_SCHEMA_VERSION)
                .await
                .ok()
                .flatten();
            if current_schema_version.as_deref() != Some(SCHEMA_VERSION) {
                drop(db);
                let _ = std::fs::remove_file(&path);
                db = build_db(&path).await?;
                db.push_schema().await?;
                set_setting_on(&mut db, SETTING_SCHEMA_VERSION, SCHEMA_VERSION).await?;
            }
        }

        let store = Self {
            path,
            config_path,
            artifact_index_dir,
            db,
            git_accounts_lock: Mutex::new(()),
            providers_cache: Mutex::new(None),
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

    pub fn artifact_index_dir(&self) -> &Path {
        &self.artifact_index_dir
    }
}
