//! 服务与离线维护共用的数据目录所有权，锁文件在释放后保留原身份。

use anyhow::{Context, Result, ensure};
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

/// 存活期间独占一个 Mai 数据目录；普通服务、检查和迁移不能同时取得所有权。
#[derive(Debug)]
pub struct DataDirectoryLease {
    _file: File,
}

impl DataDirectoryLease {
    /// 取得已存在目录的独占锁；不等待其他服务或维护进程退出。
    pub fn acquire(root: &Path) -> Result<Self> {
        ensure!(
            root.is_absolute() && root.is_dir(),
            "数据目录必须是已存在的绝对目录：{}",
            root.display()
        );
        let path = root.join(".mai-server.lock");
        if let Ok(metadata) = path.symlink_metadata() {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "数据目录锁必须是常规文件"
            );
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        file.try_lock().with_context(|| {
            format!(
                "数据目录仍由服务或维护进程持有，请先停服：{}",
                root.display()
            )
        })?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_is_exclusive_and_released_without_replacing_the_lock_file() {
        let root = tempfile::tempdir().unwrap();
        let first = DataDirectoryLease::acquire(root.path()).unwrap();
        assert!(DataDirectoryLease::acquire(root.path()).is_err());
        drop(first);
        assert!(root.path().join(".mai-server.lock").is_file());
        DataDirectoryLease::acquire(root.path()).unwrap();
    }
}
