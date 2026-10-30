//! 只同步本存储实际创建或修改的目录，不打开仅用于穿越的既有祖先。

use super::store::MaiResourceStoreError;
use std::{fs, io, path::Path};

/// 确保持久目录存在：每个新目录先同步自身，再同步包含新目录项的父目录。
pub(super) fn ensure_directory(path: &Path) -> Result<(), MaiResourceStoreError> {
    match fs::create_dir(path) {
        Ok(()) => {
            sync_directory(path)?;
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .ok_or_else(|| MaiResourceStoreError::Io {
                    path: path.to_owned(),
                    source: error,
                })?;
            ensure_directory(parent)?;
            ensure_directory(path)
        }
        Err(source) => Err(MaiResourceStoreError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// 对象原子发布只改变其直属目录，所有 I/O 错误都保留并向上传递。
pub(super) fn sync_directory(path: &Path) -> Result<(), MaiResourceStoreError> {
    #[cfg(unix)]
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| MaiResourceStoreError::Io {
            path: path.to_owned(),
            source,
        })?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
