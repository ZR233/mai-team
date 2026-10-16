//! 源码树扫描：条目清单、类型与链接安全检查、内容摘要。
//!
//! 与 [`crate::policy`] 的分工：policy 决定"哪个目录可以作为参考源"，tree 负责把这个
//! 目录读成可核对、可摘要的条目集合。树内拒绝 `.git` 组件、socket/FIFO/设备、内部子
//! 挂载与硬链接（nlink > 1）；符号链接只允许在同一参考源内部解析，不跟随遍历，断链、
//! 循环与外部链接都失败。
//!
//! 每个文件同时计算 SHA-256（内容摘要）与 Git blob 对象 ID，后者用于和可信发布
//! manifest 逐项交叉核对；目录条目不参与内容身份。

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use mai_protocol::ReferenceErrorCode;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::error::{ReferenceError, Result};
use crate::policy::unsafe_symlink;
use crate::policy::{
    ReferenceDeploymentPolicy, TreeNodeTrust, ValidatedSourceRoot, validate_tree_node_trust,
};
use crate::tree_mounts::MountTable;

/// 符号链接解析的最大层数；超过即视为循环或恶意链。
const SYMLINK_RESOLUTION_LIMIT: usize = 40;

/// 内容摘要的读取缓冲；整棵源码树共享同一块，避免每个文件重复分配并清零 1 MiB。
const HASH_BUFFER_BYTES: usize = 1024 * 1024;

/// 源码树条目种类；与发布 manifest 的 `kind` 取值一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceEntryKind {
    Directory,
    File,
    Symlink,
}

impl SourceEntryKind {
    /// 摘要与 manifest 中使用的稳定取值。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
            Self::Symlink => "symlink",
        }
    }
}

/// 扫描得到的单个条目。
///
/// 目录条目不参与内容身份，因此 `sha256` 与 `git_blob` 为空；文件与符号链接的
/// `sha256` 对内容字节计算（符号链接的内容是其链接目标的 UTF-8 字节），`git_blob`
/// 是 `sha1("blob <size>\u{0}" + 内容)`，可与可信发布 manifest 交叉核对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTreeEntry {
    pub path: String,
    pub kind: SourceEntryKind,
    pub size: u64,
    pub sha256: String,
    pub git_blob: Option<String>,
    pub executable: bool,
    pub link_target: Option<String>,
}

/// 源码树扫描结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTreeScan {
    entries: Vec<SourceTreeEntry>,
    file_count: u64,
    directory_count: u64,
    symlink_count: u64,
    byte_count: u64,
    digest: String,
}

impl SourceTreeScan {
    /// 按路径字典序排列的全部条目。
    pub fn entries(&self) -> &[SourceTreeEntry] {
        &self.entries
    }

    /// 条目总数（文件、符号链接与目录）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否存在条目；空源码树不允许登记。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 普通文件数量。
    pub fn file_count(&self) -> u64 {
        self.file_count
    }

    /// 目录数量。
    pub fn directory_count(&self) -> u64 {
        self.directory_count
    }

    /// 符号链接数量。
    pub fn symlink_count(&self) -> u64 {
        self.symlink_count
    }

    /// 参与内容身份的条目数（文件 + 符号链接），与发布 manifest 的 `file_count` 同义。
    pub fn entry_count(&self) -> u64 {
        self.file_count + self.symlink_count
    }

    /// 条目字节总量（目录为 0），与发布 manifest 的 `total_bytes` 同义。
    pub fn byte_count(&self) -> u64 {
        self.byte_count
    }

    /// 普通目录摘要（`directory_tree.v1` 摘要域）。
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// 按相对路径查找条目。
    pub fn find(&self, path: &str) -> Option<&SourceTreeEntry> {
        self.entries
            .binary_search_by(|entry| entry.path.as_str().cmp(path))
            .ok()
            .map(|index| &self.entries[index])
    }
}

/// 扫描源码树：类型、硬链接、子挂载、符号链接安全与内容摘要。
pub fn scan_source_tree(
    root: &ValidatedSourceRoot,
    policy: &ReferenceDeploymentPolicy,
) -> Result<SourceTreeScan> {
    // 子挂载只读一次挂载表：同设备 bind 挂载不会改变设备号，逐文件看 metadata.dev() 会漏。
    reject_submounts(root.path(), &MountTable::read_current()?)?;

    let mut entries: Vec<SourceTreeEntry> = Vec::new();
    let mut pending: Vec<(String, PathBuf)> = vec![(String::new(), root.path().to_path_buf())];
    let mut file_count = 0u64;
    let mut directory_count = 0u64;
    let mut symlink_count = 0u64;
    let mut byte_count = 0u64;
    let mut hash_buffer = vec![0u8; HASH_BUFFER_BYTES];

    while let Some((parent_relative, directory)) = pending.pop() {
        let children = read_sorted_children(&directory, root)?;
        for (name, path) in children {
            let relative = if parent_relative.is_empty() {
                name.clone()
            } else {
                format!("{parent_relative}/{name}")
            };
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                ReferenceError::from_io(
                    ReferenceErrorCode::SourceUnreadable,
                    format!("无法读取源码条目：{}", relative),
                    error,
                )
            })?;
            if metadata.dev() != root.device() {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::UnsafeInternalMount,
                    format!("源码树内出现子挂载：{relative}"),
                ));
            }
            validate_tree_node_trust(policy, &relative, &TreeNodeTrust::from_metadata(&metadata))?;

            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                let target = validate_symlink(root, &path, &relative)?;
                let target_bytes = target.as_bytes();
                let size = target_bytes.len() as u64;
                let sha256 = hex_sha256(target_bytes);
                let git_blob = git_blob_object_id(target_bytes);
                entries.push(SourceTreeEntry {
                    path: relative.clone(),
                    kind: SourceEntryKind::Symlink,
                    size,
                    sha256,
                    git_blob: Some(git_blob),
                    executable: false,
                    link_target: Some(target),
                });
                symlink_count += 1;
                byte_count = byte_count.checked_add(size).ok_or_else(overflow)?;
            } else if file_type.is_dir() {
                entries.push(SourceTreeEntry {
                    path: relative.clone(),
                    kind: SourceEntryKind::Directory,
                    size: 0,
                    sha256: String::new(),
                    git_blob: None,
                    executable: false,
                    link_target: None,
                });
                directory_count += 1;
                pending.push((relative, path));
            } else if file_type.is_file() {
                if metadata.nlink() > 1 {
                    return Err(ReferenceError::new(
                        ReferenceErrorCode::UnsafeHardlink,
                        format!("源码树内不允许硬链接（nlink > 1）：{relative}"),
                    ));
                }
                let digests =
                    hash_regular_file(&path, &relative, metadata.len(), &mut hash_buffer)?;
                entries.push(SourceTreeEntry {
                    path: relative.clone(),
                    kind: SourceEntryKind::File,
                    size: digests.size,
                    sha256: digests.sha256,
                    git_blob: Some(digests.git_blob),
                    executable: metadata.mode() & 0o111 != 0,
                    link_target: None,
                });
                file_count += 1;
                byte_count = byte_count.checked_add(digests.size).ok_or_else(overflow)?;
            } else {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::UnsafeFileType,
                    format!("源码树内不允许的条目类型（socket/FIFO/设备等）：{relative}"),
                ));
            }

            if entries.len() as u64 > policy.max_scan_entries() {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::SourceTooLarge,
                    format!("源码树条目数超过部署上限 {}", policy.max_scan_entries()),
                ));
            }
        }
    }

    entries.sort_by(|left, right| left.path.cmp(&right.path));
    // 目录不构成内容身份：只有空目录的树等同于空参考源，必须拒绝而不是登记一个空视图。
    if file_count + symlink_count == 0 {
        return Err(ReferenceError::new(
            ReferenceErrorCode::SourceEmpty,
            "源码树没有任何文件或符号链接（只有空目录），拒绝登记空参考源",
        ));
    }
    let digest = crate::manifest::directory_tree_digest(&entries);
    Ok(SourceTreeScan {
        entries,
        file_count,
        directory_count,
        symlink_count,
        byte_count,
        digest,
    })
}

/// 拒绝源码根严格子路径上的任何挂载点。
///
/// 与逐条目的 `metadata.dev()` 检查互补：设备号能挡住跨文件系统挂载，挂载表才能挡住
/// 同文件系统的 bind 挂载。根自身作为挂载点是允许的（允许根本来就可以是独立文件系统）。
fn reject_submounts(root: &Path, mounts: &MountTable) -> Result<()> {
    match mounts.first_mount_strictly_below(root) {
        Some(mount_point) => Err(ReferenceError::new(
            ReferenceErrorCode::UnsafeInternalMount,
            format!("源码树内出现子挂载：{}", mount_point.display()),
        )),
        None => Ok(()),
    }
}

fn read_sorted_children(
    directory: &Path,
    root: &ValidatedSourceRoot,
) -> Result<Vec<(String, PathBuf)>> {
    let read = fs::read_dir(directory).map_err(|error| {
        ReferenceError::from_io(
            ReferenceErrorCode::SourceUnreadable,
            format!(
                "无法列出源码目录：{}",
                directory
                    .strip_prefix(root.path())
                    .unwrap_or(directory)
                    .display()
            ),
            error,
        )
    })?;

    let mut children: Vec<(String, PathBuf)> = Vec::new();
    for item in read {
        let item = item.map_err(|error| {
            ReferenceError::from_io(
                ReferenceErrorCode::SourceUnreadable,
                format!("无法读取源码目录条目：{}", directory.display()),
                error,
            )
        })?;
        let name = item.file_name().into_string().map_err(|_| {
            ReferenceError::new(
                ReferenceErrorCode::UnsafePathComponent,
                format!("源码树内存在非 UTF-8 文件名：{}", directory.display()),
            )
        })?;
        if name == ".git" {
            return Err(ReferenceError::new(
                ReferenceErrorCode::UnsafePathComponent,
                format!(
                    "源码树内不允许 .git：{}",
                    directory
                        .strip_prefix(root.path())
                        .map(|relative| relative.join(&name))
                        .unwrap_or_else(|_| directory.join(&name))
                        .display()
                ),
            ));
        }
        children.push((name, item.path()));
    }
    children.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(children)
}

/// 校验一个符号链接：非空、相对、UTF-8、无控制字符，且解析后仍位于同一参考源内、
/// 终点存在且不是特殊文件；循环与过深链都失败。返回链接目标的原始文本。
fn validate_symlink(
    root: &ValidatedSourceRoot,
    link_path: &Path,
    relative: &str,
) -> Result<String> {
    let target = fs::read_link(link_path).map_err(|error| {
        ReferenceError::from_io(
            ReferenceErrorCode::SourceUnreadable,
            format!("无法读取符号链接：{relative}"),
            error,
        )
    })?;
    let target_text = target
        .to_str()
        .ok_or_else(|| unsafe_symlink(format!("符号链接目标不是 UTF-8：{relative}")))?;
    if target_text.is_empty() {
        return Err(unsafe_symlink(format!("符号链接目标为空：{relative}")));
    }
    if target_text.starts_with('/') {
        return Err(unsafe_symlink(format!(
            "符号链接目标不能是绝对路径：{relative}"
        )));
    }
    if target_text
        .chars()
        .any(|character| (character as u32) < 0x20 || (character as u32) == 0x7f)
    {
        return Err(unsafe_symlink(format!(
            "符号链接目标不能包含控制字符：{relative}"
        )));
    }
    if target_text.split('/').any(|component| component == ".git") {
        return Err(ReferenceError::new(
            ReferenceErrorCode::UnsafePathComponent,
            format!("符号链接目标不能指向 .git：{relative}"),
        ));
    }

    let parent = link_path.parent().unwrap_or(root.path());
    let mut current = parent.join(&target);
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    for _ in 0..SYMLINK_RESOLUTION_LIMIT {
        let normalized = normalize_within(root.path(), &current)
            .ok_or_else(|| unsafe_symlink(format!("符号链接逃出源码根：{relative}")))?;
        if !visited.insert(normalized.clone()) {
            return Err(unsafe_symlink(format!("符号链接出现循环：{relative}")));
        }
        let metadata = fs::symlink_metadata(&normalized)
            .map_err(|_| unsafe_symlink(format!("符号链接断链：{relative} -> {target_text}")))?;
        if metadata.dev() != root.device() {
            return Err(ReferenceError::new(
                ReferenceErrorCode::UnsafeInternalMount,
                format!("符号链接指向源码树外的挂载点：{relative}"),
            ));
        }
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            let next = fs::read_link(&normalized).map_err(|error| {
                ReferenceError::from_io(
                    ReferenceErrorCode::SourceUnreadable,
                    format!("无法读取符号链接：{relative}"),
                    error,
                )
            })?;
            current = normalized.parent().unwrap_or(root.path()).join(next);
            continue;
        }
        if !file_type.is_file() && !file_type.is_dir() {
            return Err(ReferenceError::new(
                ReferenceErrorCode::UnsafeFileType,
                format!("符号链接指向不允许的文件类型：{relative}"),
            ));
        }
        return Ok(target_text.to_string());
    }
    Err(unsafe_symlink(format!(
        "符号链接解析层数超过上限 {SYMLINK_RESOLUTION_LIMIT}：{relative}"
    )))
}

/// 词法归一化并确认仍在源码根内；出现逃逸返回 `None`。
fn normalize_within(root: &Path, path: &Path) -> Option<PathBuf> {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => parts.clear(),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Prefix(_) => return None,
            Component::Normal(name) => parts.push(name.to_os_string()),
        }
    }
    let mut normalized = PathBuf::from("/");
    for part in parts {
        normalized.push(part);
    }
    if normalized == *root || normalized.starts_with(root) {
        Some(normalized)
    } else {
        None
    }
}

struct FileDigests {
    size: u64,
    sha256: String,
    git_blob: String,
}

/// 计算单个普通文件的 SHA-256 与 Git blob 对象 ID。
///
/// `buffer` 由调用方在整次扫描中复用，避免每个文件都新分配并清零 1 MiB；只改变缓冲的
/// 生命周期，不影响任何摘要输入。
fn hash_regular_file(
    path: &Path,
    relative: &str,
    expected_size: u64,
    buffer: &mut [u8],
) -> Result<FileDigests> {
    let mut file = fs::File::open(path).map_err(|error| {
        ReferenceError::from_io(
            ReferenceErrorCode::SourceUnreadable,
            format!("无法打开源码文件：{relative}"),
            error,
        )
    })?;
    let mut sha256 = sha2::Sha256::new();
    let mut sha1 = sha1::Sha1::new();
    sha1.update(format!("blob {expected_size}\0").as_bytes());
    loop {
        let read = std::io::Read::read(&mut file, &mut *buffer).map_err(|error| {
            ReferenceError::from_io(
                ReferenceErrorCode::SourceUnreadable,
                format!("无法读取源码文件：{relative}"),
                error,
            )
        })?;
        if read == 0 {
            break;
        }
        sha256.update(&buffer[..read]);
        sha1.update(&buffer[..read]);
    }
    let size = std::fs::metadata(path)
        .map_err(|error| {
            ReferenceError::from_io(
                ReferenceErrorCode::SourceUnreadable,
                format!("无法读取源码文件元数据：{relative}"),
                error,
            )
        })?
        .len();
    if size != expected_size {
        return Err(ReferenceError::new(
            ReferenceErrorCode::SourceUnreadable,
            format!("源码文件在扫描过程中被修改：{relative}"),
        ));
    }
    let sha256_digest = sha256.finalize();
    let sha1_digest = sha1.finalize();
    Ok(FileDigests {
        size,
        sha256: format!("{sha256_digest:x}"),
        git_blob: format!("{sha1_digest:x}"),
    })
}

/// Git blob 对象 ID：`sha1("blob <size>\u{0}" + 内容)`。
fn git_blob_object_id(content: &[u8]) -> String {
    let mut hasher = sha1::Sha1::new();
    hasher.update(format!("blob {}\0", content.len()).as_bytes());
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

fn hex_sha256(content: &[u8]) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

fn overflow() -> ReferenceError {
    ReferenceError::new(ReferenceErrorCode::SourceTooLarge, "源码字节总量溢出")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{SourceTrustRequirement, validate_source_root};
    use pretty_assertions::assert_eq;

    fn test_policy(root: &Path) -> ReferenceDeploymentPolicy {
        ReferenceDeploymentPolicy::new(vec![root.to_path_buf()])
            .expect("允许根")
            .with_source_trust_requirement(SourceTrustRequirement::Unrestricted)
    }

    fn scan_fixture(root: &Path) -> (ReferenceDeploymentPolicy, ValidatedSourceRoot) {
        // 允许根必须是源码根的严格祖先，因此这里取父目录。
        let policy = test_policy(root.parent().expect("源码根的父目录"));
        let validated = validate_source_root(&policy, &root.to_string_lossy()).expect("源根");
        (policy, validated)
    }

    #[test]
    fn regular_file_git_identity_includes_object_header() {
        let temp = tempfile::tempdir().expect("临时目录");
        let root = temp.path().join("allowed/repo");
        std::fs::create_dir_all(&root).expect("源码根");
        std::fs::write(root.join("hello.txt"), b"hello\n").expect("文件");
        let (policy, validated) = scan_fixture(&root);
        let scan = scan_source_tree(&validated, &policy).expect("扫描");
        assert_eq!(
            Some("ce013625030ba8dba906f756967f9e9ca394464a"),
            scan.find("hello.txt")
                .and_then(|entry| entry.git_blob.as_deref())
        );
    }

    #[test]
    fn scan_rejects_git_directory_but_accepts_internal_symlink() {
        let temp = tempfile::tempdir().expect("临时目录");
        let root = temp.path().join("allowed").join("repo");
        std::fs::create_dir_all(&root).expect("源码根");
        std::fs::write(root.join("plain.c"), b"int fixture(void) { return 1; }\n").expect("文件");
        std::os::unix::fs::symlink("plain.c", root.join("alias.c")).expect("符号链接");
        let (policy, validated) = scan_fixture(&root);

        let scan = scan_source_tree(&validated, &policy).expect("扫描");
        assert_eq!(1, scan.file_count());
        assert_eq!(1, scan.symlink_count());
        assert_eq!(2, scan.entry_count());
        assert!(scan.digest().starts_with("sha256:"));
        let link = scan.find("alias.c").expect("符号链接条目");
        assert_eq!(SourceEntryKind::Symlink, link.kind);
        assert_eq!(Some("plain.c".to_string()), link.link_target);
        assert_eq!("plain.c".len() as u64, link.size);

        std::fs::create_dir_all(root.join(".git")).expect(".git 目录");
        assert_eq!(
            ReferenceErrorCode::UnsafePathComponent,
            scan_source_tree(&validated, &policy)
                .expect_err(".git 必须被拒绝")
                .code()
        );
    }

    #[test]
    fn scan_rejects_escaping_and_dangling_symlinks() {
        let temp = tempfile::tempdir().expect("临时目录");
        let root = temp.path().join("allowed").join("repo");
        std::fs::create_dir_all(&root).expect("源码根");
        std::fs::write(root.join("plain.c"), b"x\n").expect("文件");
        std::fs::write(temp.path().join("outside.c"), b"secret\n").expect("外部文件");
        let (policy, validated) = scan_fixture(&root);

        std::os::unix::fs::symlink("../outside.c", root.join("escape.c")).expect("逃逸链接");
        assert_eq!(
            ReferenceErrorCode::UnsafeSymlink,
            scan_source_tree(&validated, &policy)
                .expect_err("逃逸必须失败")
                .code()
        );
        std::fs::remove_file(root.join("escape.c")).expect("清理");

        std::os::unix::fs::symlink("/etc/passwd", root.join("absolute.c")).expect("绝对链接");
        assert_eq!(
            ReferenceErrorCode::UnsafeSymlink,
            scan_source_tree(&validated, &policy)
                .expect_err("绝对链接必须失败")
                .code()
        );
        std::fs::remove_file(root.join("absolute.c")).expect("清理");

        std::os::unix::fs::symlink("a", root.join("a")).expect("自环链接");
        assert_eq!(
            ReferenceErrorCode::UnsafeSymlink,
            scan_source_tree(&validated, &policy)
                .expect_err("循环必须失败")
                .code()
        );
        std::fs::remove_file(root.join("a")).expect("清理");

        std::os::unix::fs::symlink("does-not-exist", root.join("dangling")).expect("断链");
        assert_eq!(
            ReferenceErrorCode::UnsafeSymlink,
            scan_source_tree(&validated, &policy)
                .expect_err("断链必须失败")
                .code()
        );
    }

    #[test]
    fn scan_rejects_hardlinks_and_empty_tree() {
        let temp = tempfile::tempdir().expect("临时目录");
        let root = temp.path().join("allowed").join("repo");
        std::fs::create_dir_all(&root).expect("源码根");
        let (policy, validated) = scan_fixture(&root);

        assert_eq!(
            ReferenceErrorCode::SourceEmpty,
            scan_source_tree(&validated, &policy)
                .expect_err("空源码树")
                .code()
        );

        std::fs::write(root.join("plain.c"), b"x\n").expect("文件");
        std::fs::hard_link(root.join("plain.c"), root.join("hard.c")).expect("硬链接");
        assert_eq!(
            ReferenceErrorCode::UnsafeHardlink,
            scan_source_tree(&validated, &policy)
                .expect_err("硬链接必须失败")
                .code()
        );
    }

    #[test]
    fn scan_honours_entry_limit() {
        let temp = tempfile::tempdir().expect("临时目录");
        let root = temp.path().join("allowed").join("repo");
        std::fs::create_dir_all(&root).expect("源码根");
        std::fs::write(root.join("a.c"), b"x\n").expect("文件");
        std::fs::write(root.join("b.c"), b"y\n").expect("文件");
        let policy = test_policy(root.parent().expect("源码根的父目录")).with_max_scan_entries(1);
        let validated = validate_source_root(&policy, &root.to_string_lossy()).expect("源根");
        assert_eq!(
            ReferenceErrorCode::SourceTooLarge,
            scan_source_tree(&validated, &policy)
                .expect_err("超过上限")
                .code()
        );
    }

    #[test]
    fn reject_submounts_uses_mount_table_and_component_boundaries() {
        // 同文件系统（ext4 8:1）的 bind 挂载：设备号与源码根相同，只有挂载表能发现。
        let mounts = MountTable::parse(
            "52 41 8:1 /vendor /srv/mai-references/linux/repo/vendor ro,relatime - ext4 /dev/sda1 ro",
        )
        .expect("挂载表");
        let error = reject_submounts(Path::new("/srv/mai-references/linux/repo"), &mounts)
            .expect_err("源码树内的同设备 bind 挂载必须被拒绝");
        assert_eq!(ReferenceErrorCode::UnsafeInternalMount, error.code());
        assert!(
            error
                .message()
                .contains("/srv/mai-references/linux/repo/vendor")
        );

        // 名字相近但不是子路径的挂载不能误判。
        let lookalike = MountTable::parse(
            "53 41 8:1 /evil /srv/mai-references/linux/repo-evil ro,relatime - ext4 /dev/sda1 ro",
        )
        .expect("挂载表");
        assert!(reject_submounts(Path::new("/srv/mai-references/linux/repo"), &lookalike).is_ok());

        // 源码根自身作为挂载点（允许根是独立文件系统）不是子挂载。
        let root_mount = MountTable::parse(
            "54 41 8:1 / /srv/mai-references/linux/repo rw,relatime - ext4 /dev/sda1 rw",
        )
        .expect("挂载表");
        assert!(reject_submounts(Path::new("/srv/mai-references/linux/repo"), &root_mount).is_ok());
    }

    #[test]
    fn scan_rejects_tree_with_only_empty_directories() {
        let temp = tempfile::tempdir().expect("临时目录");
        let root = temp.path().join("allowed").join("repo");
        std::fs::create_dir_all(root.join("empty/nested")).expect("空目录");
        std::fs::create_dir_all(root.join("another")).expect("空目录");
        let (policy, validated) = scan_fixture(&root);

        assert_eq!(
            ReferenceErrorCode::SourceEmpty,
            scan_source_tree(&validated, &policy)
                .expect_err("只有空目录的树必须被拒绝")
                .code()
        );

        std::fs::write(root.join("empty/plain.c"), b"x\n").expect("文件");
        let scan = scan_source_tree(&validated, &policy).expect("有文件后可以扫描");
        assert_eq!(1, scan.file_count());
        assert_eq!(0, scan.symlink_count());
        assert_eq!(3, scan.directory_count());
    }

    #[test]
    fn shared_hash_buffer_hashes_every_file_completely() {
        let temp = tempfile::tempdir().expect("临时目录");
        let root = temp.path().join("allowed").join("repo");
        std::fs::create_dir_all(&root).expect("源码根");
        // 内容超过共享缓冲（1 MiB），且后半段不与前半段重复，能发现缓冲复用/残留错误。
        let mut content = Vec::with_capacity(1_400_000);
        for index in 0..1_400_000u32 {
            content.push((index % 251) as u8);
        }
        std::fs::write(root.join("big.bin"), &content).expect("大文件");
        std::fs::write(root.join("small.bin"), b"tail\n").expect("小文件");
        let (policy, validated) = scan_fixture(&root);

        let scan = scan_source_tree(&validated, &policy).expect("扫描");
        let big = scan.find("big.bin").expect("大文件条目");
        assert_eq!(content.len() as u64, big.size);
        assert_eq!(hex_sha256(&content), big.sha256);

        let mut blob = sha1::Sha1::new();
        blob.update(format!("blob {}\0", content.len()).as_bytes());
        blob.update(&content);
        assert_eq!(Some(format!("{:x}", blob.finalize())), big.git_blob);

        let small = scan.find("small.bin").expect("小文件条目");
        assert_eq!(hex_sha256(b"tail\n"), small.sha256);
    }
}
