use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use pl_core::{
    context::{ResourceError, ResourceReadError, ResourceReader, ResourceReference},
    thread::cold::{ColdStoreError, OutputStorageFault, StorageFaultKind},
    tool::opaque::ToolError,
};
use tokio_util::sync::CancellationToken;

/// 本存储拥有的资源 id 前缀；只有本模块成功写入后才会生成这种引用。
pub(crate) const RESOURCE_ID_PREFIX: &str = "mai.thread.resource:";

/// 单个命令归档允许复制的最大字节数。
///
/// 该上限只约束命令输出归档，不改变 `ResourceReader` 读取既有对象的能力。复制在 64KiB 分块上
/// 逐块校验，超过上限的 capture 不会被写入第二个不受限的副本。
pub(crate) const MAX_COMMAND_ARCHIVE_BYTES: u64 = 32 * 1024 * 1024;

/// 产品 Thread 资源目录名；调用方用它把 artifact 根解析成注入给 [`MaiResourceStore`] 的根。
pub(crate) const THREAD_RESOURCES_DIR: &str = "thread-resources";

const COMMAND_CAPTURE_MEDIA_TYPE: &str = "application/octet-stream";
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const SHA256_PREFIX: &str = "sha256:";

/// 把 artifact 根转换为本存储使用的产品级资源根。
pub(crate) fn thread_resources_root(artifact_files_root: &Path) -> PathBuf {
    artifact_files_root.join(THREAD_RESOURCES_DIR)
}

/// 内容寻址的 Thread 资源存储。
///
/// 对象文件名就是完整 SHA-256 摘要，因此相同字节在重试时解析到同一对象；克隆共享同一根目录，
/// 不会因为 Thread 生命周期结束而删除另一个 Thread 的字节。存储只按摘要命名对象、不保存每个
/// 对象的 MIME，所以相同字节以不同 media type 保留时共享同一对象，各自引用携带自己的 MIME。
#[derive(Debug, Clone)]
pub(crate) struct MaiResourceStore {
    root: Arc<PathBuf>,
}

impl MaiResourceStore {
    /// 选择持久化根目录；不创建目录、不读取配置。
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root: Arc::new(root),
        }
    }

    /// 保留一份任意外部字节（MCP 媒体等），返回仅由本存储生成的内容寻址引用。
    ///
    /// `media_type` 必须是 `type/subtype` 形式的 MIME 类型。身份只来自字节摘要，因此相同字节在
    /// 不同 MIME 下解析到同一对象，引用各自携带自己的 MIME，不会互相冲突；该路径不施加命令归档
    /// 上限，因为调用方已经把精确字节读进内存。
    ///
    /// # Errors
    /// 返回 [`MaiResourceStoreError::UnsupportedMediaType`] 表示 MIME 畸形；磁盘写入、同步或
    /// 原子发布失败返回 [`MaiResourceStoreError::Io`]；已有同摘要对象损坏时返回
    /// [`MaiResourceStoreError::Integrity`]，且绝不覆盖历史字节。
    pub(crate) async fn retain_bytes(
        &self,
        bytes: Arc<[u8]>,
        media_type: &str,
    ) -> Result<ResourceReference, MaiResourceStoreError> {
        let media_type = media_type.to_owned();
        let root = Arc::clone(&self.root);
        tokio::task::spawn_blocking(move || retain_bytes(root.as_path(), &bytes, &media_type))
            .await?
    }

    /// 保留一份完整的命令输出 capture，并返回仅由本存储生成的内容寻址引用。
    ///
    /// # Errors
    /// 返回 [`MaiResourceStoreError::TooLarge`] 表示 capture 超过
    /// [`MAX_COMMAND_ARCHIVE_BYTES`]；普通 I/O、同步或原子发布失败时返回
    /// [`MaiResourceStoreError::Io`]，调用方必须把它们映射成
    /// [`OutputStorageFault`] 而不是普通工具错误。源 capture 在所有失败分支上都会保留。
    pub(crate) async fn retain_command_capture(
        &self,
        source: &Path,
    ) -> Result<ResourceReference, MaiResourceStoreError> {
        self.retain_command_capture_with_limit(source, MAX_COMMAND_ARCHIVE_BYTES)
            .await
    }

    async fn retain_command_capture_with_limit(
        &self,
        source: &Path,
        limit: u64,
    ) -> Result<ResourceReference, MaiResourceStoreError> {
        let source = source.to_owned();
        let root = Arc::clone(&self.root);
        tokio::task::spawn_blocking(move || retain_command_capture(root.as_path(), &source, limit))
            .await?
    }
}

/// 本存储的写入、身份和读取错误。
///
/// [`MaiResourceStoreError::Io`] 代表字节没有被可靠保存；超限、非普通文件、畸形 MIME、身份与
/// 摘要拒绝都是策略性失败，源 capture 保持原样，调用方不应把它们升级为 Thread 级存储故障。
#[derive(Debug, thiserror::Error)]
pub(crate) enum MaiResourceStoreError {
    #[error("thread resource store I/O failed at {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("command capture source is not a regular file: {0}")]
    NotFile(PathBuf),
    #[error("command capture exceeds the {limit}-byte archive limit")]
    TooLarge { limit: u64 },
    #[error("resource identity is not owned by the mai thread store")]
    Identity,
    #[error("resource media type is not a supported MIME type: {0}")]
    UnsupportedMediaType(String),
    #[error("resource metadata or bytes failed integrity validation")]
    Integrity(#[from] ResourceError),
    #[error("thread resource store worker failed")]
    Worker(#[from] tokio::task::JoinError),
}

impl ResourceReader for MaiResourceStore {
    async fn read(
        &self,
        reference: ResourceReference,
        cancellation: CancellationToken,
    ) -> Result<Arc<[u8]>, ResourceReadError> {
        if cancellation.is_cancelled() {
            return Err(ResourceReadError::Cancelled);
        }
        reference.validate()?;

        let root_path = self.root.as_ref().clone();
        let root = tokio::fs::canonicalize(root_path.as_path()).await;
        let root = root.map_err(|source| unavailable(io_error(root_path, source)))?;
        let path = resource_path(&root, &reference).map_err(unavailable)?;

        // 根目录已经规范化，文件名又只能是 64 位小写摘要；这里再拒绝最终对象的符号链接，
        // 防止根目录被替换或摘要槽位被伪造为外部文件。
        let metadata = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|source| unavailable(io_error(path.clone(), source)))?;
        if !metadata.file_type().is_file() {
            return Err(unavailable(io_error(
                path,
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "resource object is not a regular file",
                ),
            )));
        }
        if metadata.len() != reference.byte_len() {
            return Err(ResourceReadError::Integrity(ResourceError::ContentMismatch));
        }

        let canonical = tokio::fs::canonicalize(&path).await;
        let canonical = canonical.map_err(|source| unavailable(io_error(path, source)))?;
        if !canonical.starts_with(&root) {
            return Err(unavailable(io_error(
                canonical,
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "resource path escapes the thread resource root",
                ),
            )));
        }

        let bytes = tokio::fs::read(&canonical).await;
        let bytes = bytes.map_err(|source| unavailable(io_error(canonical, source)))?;
        if cancellation.is_cancelled() {
            return Err(ResourceReadError::Cancelled);
        }
        reference.verify(&bytes)?;
        Ok(Arc::from(bytes))
    }
}

impl pl_tool::exec::CommandOutputArchive for MaiResourceStore {
    async fn retain(
        &self,
        _thread_id: &str,
        snapshot: &pl_tool::command::CommandOutputSnapshot,
    ) -> Result<ResourceReference, ToolError> {
        // 身份只来自归档字节的摘要，不来自 Thread/call 字符串；调用方不需要、也不允许用这些
        // 字符串参与文件系统路径。
        self.retain_command_capture(&snapshot.capture_file)
            .await
            .map_err(MaiResourceStoreError::tool_error)
    }
}

impl MaiResourceStoreError {
    /// 把存储失败映射到 PL 工具边界。
    ///
    /// 真正的磁盘写入、同步或原子发布失败会包装成 core 能识别的
    /// [`OutputStorageFault`]；超限、非普通文件、畸形 MIME、身份/摘要拒绝只是普通工具错误，
    /// 不会让 Thread 误以为整个输出存储都不可用。
    pub(crate) fn tool_error(self) -> ToolError {
        match self {
            MaiResourceStoreError::Io { source, .. } => blob_storage_fault(source),
            MaiResourceStoreError::NotFile(path) => {
                ToolError::new(MaiResourceStoreError::NotFile(path))
            }
            MaiResourceStoreError::TooLarge { limit } => {
                ToolError::new(MaiResourceStoreError::TooLarge { limit })
            }
            MaiResourceStoreError::UnsupportedMediaType(media_type) => {
                ToolError::new(MaiResourceStoreError::UnsupportedMediaType(media_type))
            }
            MaiResourceStoreError::Identity => ToolError::new(MaiResourceStoreError::Identity),
            MaiResourceStoreError::Integrity(error) => {
                ToolError::new(MaiResourceStoreError::Integrity(error))
            }
            MaiResourceStoreError::Worker(error) => {
                ToolError::new(MaiResourceStoreError::Worker(error))
            }
        }
    }
}

fn blob_storage_fault(error: io::Error) -> ToolError {
    let source = ColdStoreError {
        source: Box::new(error),
    };
    ToolError::new(OutputStorageFault::new(
        StorageFaultKind::BlobFailed,
        Arc::new(source),
    ))
}

fn retain_command_capture(
    root: &Path,
    source: &Path,
    limit: u64,
) -> Result<ResourceReference, MaiResourceStoreError> {
    let source_path = source.to_owned();
    let source_metadata =
        fs::metadata(&source_path).map_err(|source| io_error(source_path.clone(), source))?;
    if !source_metadata.is_file() {
        return Err(MaiResourceStoreError::NotFile(source_path));
    }
    if source_metadata.len() > limit {
        return Err(MaiResourceStoreError::TooLarge { limit });
    }

    fs::create_dir_all(root).map_err(|source| io_error(root.to_owned(), source))?;
    let root = fs::canonicalize(root).map_err(|source| io_error(root.to_owned(), source))?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(&root).map_err(|source| io_error(root.clone(), source))?;
    let mut input =
        File::open(&source_path).map_err(|source| io_error(source_path.clone(), source))?;

    let mut content = Vec::new();
    let mut length = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|source| io_error(source_path.clone(), source))?;
        if count == 0 {
            break;
        }
        length = length
            .checked_add(count as u64)
            .ok_or(MaiResourceStoreError::TooLarge { limit })?;
        if length > limit {
            return Err(MaiResourceStoreError::TooLarge { limit });
        }
        temporary
            .write_all(&buffer[..count])
            .map_err(|source| io_error(root.clone(), source))?;
        content.extend_from_slice(&buffer[..count]);
    }

    let reference = content_reference(COMMAND_CAPTURE_MEDIA_TYPE, &content)?;
    publish_object(&root, temporary, &reference, &content)?;
    Ok(reference)
}

/// 保留一份精确的内存字节；与命令归档共享同一套内容寻址和持久化语义。
fn retain_bytes(
    root: &Path,
    bytes: &[u8],
    media_type: &str,
) -> Result<ResourceReference, MaiResourceStoreError> {
    let reference = content_reference(media_type, bytes)?;
    fs::create_dir_all(root).map_err(|source| io_error(root.to_owned(), source))?;
    let root = fs::canonicalize(root).map_err(|source| io_error(root.to_owned(), source))?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(&root).map_err(|source| io_error(root.clone(), source))?;
    temporary
        .write_all(bytes)
        .map_err(|source| io_error(root.clone(), source))?;
    publish_object(&root, temporary, &reference, bytes)?;
    Ok(reference)
}

/// 由精确字节、长度和声明的 MIME 构造仅属于本存储的内容寻址引用。
fn content_reference(
    media_type: &str,
    content: &[u8],
) -> Result<ResourceReference, MaiResourceStoreError> {
    validate_media_type(media_type)?;
    let content_digest = pl_core::context::content_hash(content);
    let hash = content_digest
        .strip_prefix(SHA256_PREFIX)
        .ok_or(MaiResourceStoreError::Integrity(
            ResourceError::InvalidDigest,
        ))?
        .to_owned();
    ResourceReference::new(
        format!("{RESOURCE_ID_PREFIX}{hash}"),
        content_digest,
        content.len() as u64,
        media_type.to_owned(),
    )
    .map_err(Into::into)
}

/// 把已经写完的临时对象同步、按摘要原子发布到最终位置，或验证同摘要的历史对象。
fn publish_object(
    root: &Path,
    temporary: tempfile::NamedTempFile,
    reference: &ResourceReference,
    expected: &[u8],
) -> Result<(), MaiResourceStoreError> {
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| io_error(root.to_owned(), source))?;
    let destination = resource_path(root, reference)?;
    match temporary.persist_noclobber(&destination) {
        Ok(_) => {}
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            // 同摘要对象只允许验证，不允许覆盖；历史对象损坏时保留它供诊断。
            verify_existing_file(&destination, expected)?;
        }
        Err(error) => {
            return Err(io_error(destination, error.error));
        }
    }
    sync_parent_directories(root)?;
    Ok(())
}

/// 校验 media type 是 `type/subtype` 形式的 MIME。
///
/// 参数段（`;` 之后）允许出现，但只校验其本质类型。该存储只用摘要命名对象，media type 不进
/// 路径；这里拒绝空段和非 token 字符，既保证引用携带的 MIME 可被模型适配器正确解释，又避免把
/// 畸形字符串写进引用。
fn validate_media_type(media_type: &str) -> Result<(), MaiResourceStoreError> {
    let essence = media_type.split(';').next().unwrap_or_default().trim();
    let mut segments = essence.split('/');
    let (Some(type_), Some(subtype), None) = (segments.next(), segments.next(), segments.next())
    else {
        return Err(MaiResourceStoreError::UnsupportedMediaType(
            media_type.to_owned(),
        ));
    };
    if is_mime_token(type_) && is_mime_token(subtype) {
        Ok(())
    } else {
        Err(MaiResourceStoreError::UnsupportedMediaType(
            media_type.to_owned(),
        ))
    }
}

fn is_mime_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                )
        })
}

fn resource_path(
    root: &Path,
    reference: &ResourceReference,
) -> Result<PathBuf, MaiResourceStoreError> {
    reference.validate()?;
    validate_media_type(reference.media_type())?;
    let hash = reference
        .id()
        .strip_prefix(RESOURCE_ID_PREFIX)
        .ok_or(MaiResourceStoreError::Identity)?;
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || reference.content_digest() != format!("{SHA256_PREFIX}{hash}")
    {
        return Err(MaiResourceStoreError::Identity);
    }
    Ok(root.join(hash))
}

fn verify_existing_file(path: &Path, expected: &[u8]) -> Result<(), MaiResourceStoreError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| io_error(path.to_owned(), source))?;
    if !metadata.file_type().is_file() || metadata.len() != expected.len() as u64 {
        return Err(MaiResourceStoreError::Integrity(
            ResourceError::ContentMismatch,
        ));
    }

    let mut input = File::open(path).map_err(|source| io_error(path.to_owned(), source))?;
    let mut offset = 0_usize;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|source| io_error(path.to_owned(), source))?;
        if count == 0 {
            break;
        }
        let end = offset
            .checked_add(count)
            .ok_or(MaiResourceStoreError::Integrity(
                ResourceError::ContentMismatch,
            ))?;
        if end > expected.len() || buffer[..count] != expected[offset..end] {
            return Err(MaiResourceStoreError::Integrity(
                ResourceError::ContentMismatch,
            ));
        }
        offset = end;
    }
    if offset != expected.len() {
        return Err(MaiResourceStoreError::Integrity(
            ResourceError::ContentMismatch,
        ));
    }
    Ok(())
}

fn sync_parent_directories(root: &Path) -> Result<(), MaiResourceStoreError> {
    #[cfg(unix)]
    {
        for directory in root.ancestors() {
            File::open(directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|source| io_error(directory.to_owned(), source))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = root;
    }
    Ok(())
}

fn unavailable(source: MaiResourceStoreError) -> ResourceReadError {
    ResourceReadError::Unavailable {
        source: Box::new(source),
    }
}

fn io_error(path: PathBuf, source: io::Error) -> MaiResourceStoreError {
    MaiResourceStoreError::Io { path, source }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn store_in(temp: &tempfile::TempDir) -> (MaiResourceStore, PathBuf) {
        let root = thread_resources_root(temp.path());
        (MaiResourceStore::new(root.clone()), root)
    }

    #[tokio::test]
    async fn archives_and_reads_exact_command_capture() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let (store, root) = store_in(&temp);
        let source = temp.path().join("capture.log");
        let content = b"=== COMMAND ===\nprintf hello\n";
        std::fs::write(&source, content).expect("capture written");

        let reference = store
            .retain_command_capture(&source)
            .await
            .expect("capture archived");
        let expected_digest = pl_core::context::content_hash(content);
        assert_eq!(reference.byte_len(), content.len() as u64);
        assert_eq!(reference.content_digest(), expected_digest.as_str());
        assert_eq!(reference.media_type(), COMMAND_CAPTURE_MEDIA_TYPE);

        let hash = reference
            .id()
            .strip_prefix(RESOURCE_ID_PREFIX)
            .expect("store-owned identity");
        assert_eq!(
            hash,
            expected_digest
                .strip_prefix(SHA256_PREFIX)
                .expect("sha256 digest prefix")
        );
        assert!(root.join(hash).is_file(), "object is named by its digest");

        let bytes = ResourceReader::read(&store, reference.clone(), CancellationToken::new())
            .await
            .expect("resource read");
        assert_eq!(&bytes[..], &content[..]);
        assert_eq!(
            std::fs::read(&source).expect("capture retained").as_slice(),
            &content[..]
        );
    }

    #[tokio::test]
    async fn read_rejects_tampered_object_bytes() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let (store, root) = store_in(&temp);
        let source = temp.path().join("capture.log");
        std::fs::write(&source, b"abc").expect("capture written");
        let reference = store
            .retain_command_capture(&source)
            .await
            .expect("capture archived");

        let hash = reference
            .id()
            .strip_prefix(RESOURCE_ID_PREFIX)
            .expect("store-owned identity");
        std::fs::write(root.join(hash), b"bad").expect("object tampered");

        let error = ResourceReader::read(&store, reference, CancellationToken::new())
            .await
            .expect_err("tampered bytes must fail");
        assert!(
            matches!(
                &error,
                ResourceReadError::Integrity(ResourceError::ContentMismatch)
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn command_archive_limit_accepts_boundary_and_rejects_excess() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let (store, _) = store_in(&temp);
        let source = temp.path().join("capture.log");

        std::fs::write(&source, b"abc").expect("boundary capture written");
        let reference = store
            .retain_command_capture_with_limit(&source, 3)
            .await
            .expect("exact-limit capture is accepted");
        assert_eq!(reference.byte_len(), 3);

        std::fs::write(&source, b"abcd").expect("oversized capture written");
        let error = store
            .retain_command_capture_with_limit(&source, 3)
            .await
            .expect_err("capture past the limit must be refused");
        match &error {
            MaiResourceStoreError::TooLarge { limit } => assert_eq!(*limit, 3),
            other => panic!("expected too-large refusal, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(&source).expect("source retained"),
            b"abcd".to_vec()
        );
    }

    #[tokio::test]
    async fn repeated_command_archive_is_idempotent() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let (store, root) = store_in(&temp);
        let source = temp.path().join("capture.log");
        std::fs::write(&source, b"accepted capture fragment").expect("capture written");

        let first = store
            .retain_command_capture(&source)
            .await
            .expect("first archive stores the fragment");
        let retry = store
            .retain_command_capture(&source)
            .await
            .expect("retry stores the same fragment");

        assert_eq!(first, retry);
        let entries = std::fs::read_dir(&root)
            .expect("resource root readable")
            .collect::<Result<Vec<_>, _>>()
            .expect("resource entries readable");
        assert_eq!(entries.len(), 1, "retry must not create a second object");
    }

    #[tokio::test]
    async fn retry_refuses_corrupt_existing_object_without_overwrite() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let (store, root) = store_in(&temp);
        let source = temp.path().join("capture.log");
        std::fs::write(&source, b"abc").expect("capture written");
        let reference = store
            .retain_command_capture(&source)
            .await
            .expect("capture archived");

        let hash = reference
            .id()
            .strip_prefix(RESOURCE_ID_PREFIX)
            .expect("store-owned identity");
        let object = root.join(hash);
        std::fs::write(&object, b"bad").expect("existing object corrupted");

        let error = store
            .retain_command_capture(&source)
            .await
            .expect_err("retry must verify the existing object");
        assert!(
            matches!(
                &error,
                MaiResourceStoreError::Integrity(ResourceError::ContentMismatch)
            ),
            "{error:?}"
        );
        assert_eq!(
            std::fs::read(&object).expect("corrupt object retained"),
            b"bad".to_vec()
        );
    }

    #[tokio::test]
    async fn reader_rejects_forged_or_traversal_identity() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let (_store, root) = store_in(&temp);
        std::fs::create_dir_all(&root).expect("resource root created");
        let store = MaiResourceStore::new(root);
        let digest = pl_core::context::content_hash(b"outside");

        let traversal = ResourceReference::new(
            "../outside".to_string(),
            digest.clone(),
            7,
            COMMAND_CAPTURE_MEDIA_TYPE.to_string(),
        )
        .expect("valid reference metadata");
        let error = ResourceReader::read(&store, traversal, CancellationToken::new())
            .await
            .expect_err("path traversal identity must be refused");
        assert!(
            matches!(&error, ResourceReadError::Unavailable { .. }),
            "{error:?}"
        );

        let forged = ResourceReference::new(
            format!("{RESOURCE_ID_PREFIX}{}", "0".repeat(64)),
            digest,
            7,
            COMMAND_CAPTURE_MEDIA_TYPE.to_string(),
        )
        .expect("valid reference metadata");
        let error = ResourceReader::read(&store, forged, CancellationToken::new())
            .await
            .expect_err("forged identity must be refused");
        assert!(
            matches!(&error, ResourceReadError::Unavailable { .. }),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn retains_and_reads_arbitrary_media_type() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let (store, root) = store_in(&temp);
        let bytes: Arc<[u8]> = Arc::from(b"\x89PNG\r\n\x1a\nmcp-image".to_vec());

        let reference = store
            .retain_bytes(Arc::clone(&bytes), "image/png")
            .await
            .expect("media retained");
        assert_eq!(reference.media_type(), "image/png");
        assert_eq!(reference.byte_len(), bytes.len() as u64);
        assert_eq!(
            reference.content_digest(),
            pl_core::context::content_hash(&bytes).as_str()
        );
        let hash = reference
            .id()
            .strip_prefix(RESOURCE_ID_PREFIX)
            .expect("store-owned identity");
        assert!(root.join(hash).is_file(), "object is named by its digest");

        let read = ResourceReader::read(&store, reference, CancellationToken::new())
            .await
            .expect("media read back");
        assert_eq!(&read[..], &bytes[..]);
    }

    #[tokio::test]
    async fn same_bytes_under_two_media_types_share_one_object() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let (store, root) = store_in(&temp);
        let bytes: Arc<[u8]> = Arc::from(b"identical bytes, declared twice".to_vec());

        let as_blob = store
            .retain_bytes(Arc::clone(&bytes), "application/octet-stream")
            .await
            .expect("blob retained");
        let as_image = store
            .retain_bytes(Arc::clone(&bytes), "image/png")
            .await
            .expect("image declared over the same bytes");

        assert_eq!(
            as_blob.id(),
            as_image.id(),
            "identity is the digest alone; the MIME must not fragment the object"
        );
        assert_eq!(as_blob.media_type(), "application/octet-stream");
        assert_eq!(as_image.media_type(), "image/png");
        let entries = std::fs::read_dir(&root)
            .expect("resource root readable")
            .collect::<Result<Vec<_>, _>>()
            .expect("resource entries readable");
        assert_eq!(entries.len(), 1, "one digest stores one object");

        for reference in [as_blob, as_image] {
            let read = ResourceReader::read(&store, reference, CancellationToken::new())
                .await
                .expect("each MIME reads the shared bytes back");
            assert_eq!(&read[..], &bytes[..]);
        }
    }

    #[tokio::test]
    async fn repeated_media_retention_is_idempotent() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let (store, root) = store_in(&temp);
        let bytes: Arc<[u8]> = Arc::from(b"mcp-audio".to_vec());

        let first = store
            .retain_bytes(Arc::clone(&bytes), "audio/mpeg")
            .await
            .expect("audio retained");
        let retry = store
            .retain_bytes(Arc::clone(&bytes), "audio/mpeg")
            .await
            .expect("retry retains the same audio");
        assert_eq!(first, retry);
        let entries = std::fs::read_dir(&root)
            .expect("resource root readable")
            .collect::<Result<Vec<_>, _>>()
            .expect("resource entries readable");
        assert_eq!(entries.len(), 1, "retry must not create a second object");
    }

    #[tokio::test]
    async fn read_rejects_tampered_media_object_bytes() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let (store, root) = store_in(&temp);
        let bytes: Arc<[u8]> = Arc::from(b"mcp-blob".to_vec());
        let reference = store
            .retain_bytes(Arc::clone(&bytes), "application/pdf")
            .await
            .expect("blob retained");

        let hash = reference
            .id()
            .strip_prefix(RESOURCE_ID_PREFIX)
            .expect("store-owned identity");
        std::fs::write(root.join(hash), b"tampered").expect("object tampered");

        let error = ResourceReader::read(&store, reference, CancellationToken::new())
            .await
            .expect_err("tampered media bytes must fail");
        assert!(
            matches!(
                &error,
                ResourceReadError::Integrity(_) | ResourceReadError::Unavailable { .. }
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn refuses_malformed_media_type() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let (store, _) = store_in(&temp);
        let bytes: Arc<[u8]> = Arc::from(b"payload".to_vec());

        for media_type in [
            "not-a-mime",
            "image/",
            "/png",
            "image/png/extra",
            "image/svg xml",
        ] {
            let error = store
                .retain_bytes(Arc::clone(&bytes), media_type)
                .await
                .expect_err("malformed MIME must be refused");
            assert!(
                matches!(&error, MaiResourceStoreError::UnsupportedMediaType(_)),
                "{media_type}: {error:?}"
            );
        }
    }

    #[test]
    fn disk_io_failure_is_reported_as_blob_storage_fault() {
        let error = MaiResourceStoreError::Io {
            path: PathBuf::from("thread-resources"),
            source: io::Error::other("disk full"),
        }
        .tool_error();
        let fault = error
            .source
            .downcast_ref::<OutputStorageFault>()
            .expect("disk failure is a typed storage fault");
        assert_eq!(fault.kind, StorageFaultKind::BlobFailed);

        let refusal = MaiResourceStoreError::TooLarge {
            limit: MAX_COMMAND_ARCHIVE_BYTES,
        }
        .tool_error();
        assert!(
            refusal
                .source
                .downcast_ref::<OutputStorageFault>()
                .is_none(),
            "size refusal must stay a plain tool error"
        );

        let format_refusal =
            MaiResourceStoreError::NotFile(PathBuf::from("capture.log")).tool_error();
        assert!(
            format_refusal
                .source
                .downcast_ref::<OutputStorageFault>()
                .is_none(),
            "format refusal must stay a plain tool error"
        );

        let mime_refusal =
            MaiResourceStoreError::UnsupportedMediaType("not-a-mime".to_string()).tool_error();
        assert!(
            mime_refusal
                .source
                .downcast_ref::<OutputStorageFault>()
                .is_none(),
            "a malformed MIME refusal must stay a plain tool error"
        );
    }
}
