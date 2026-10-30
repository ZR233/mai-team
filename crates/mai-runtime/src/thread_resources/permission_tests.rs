//! 真实的仅穿越祖先目录不能成为资源发布的额外读权限要求。

#[cfg(unix)]
#[tokio::test]
async fn resource_publication_does_not_open_unchanged_ancestors() {
    use super::store::MaiResourceStore;
    use std::{fs, os::unix::fs::PermissionsExt, sync::Arc};

    let directory = tempfile::tempdir().unwrap();
    let ancestor = directory.path().join("traverse-only");
    let owned = ancestor.join("owned");
    fs::create_dir_all(&owned).unwrap();
    let source = owned.join("capture.log");
    fs::write(&source, "完整命令结果\n".as_bytes()).unwrap();
    fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o111)).unwrap();
    let store = MaiResourceStore::new(owned.join("new-parent/resources"));
    let bytes = store
        .retain_bytes(
            Arc::from(b"media bytes".as_slice()),
            "application/octet-stream",
        )
        .await;
    let capture = store.retain_command_capture(&source).await;
    // 恢复权限后再断言，任何失败也不让 TempDir 的清理依赖读不了的祖先。
    fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o700)).unwrap();
    bytes.expect("对象发布只同步自己创建或修改的目录");
    capture.expect("命令归档不读取既有祖先目录");
}
