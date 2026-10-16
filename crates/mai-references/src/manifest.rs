//! 发布器 manifest 的解析、严格核对与规范摘要计算。
//!
//! 摘要互操作规则（与随包 `scripts/publish-reference-source.py` 一致）：
//!
//! ```text
//! manifest_digest = "sha256:" + SHA256(UTF8(JSON(
//!     {"entries": entries, "format": "mai.reference.tree.v1"},
//!     sort_keys=true, separators=(",", ":"), ensure_ascii=false)))
//! ```
//!
//! entries 按路径字典序排列，每项固定字段 `git_blob`、`git_mode`、`kind`、`path`、
//! `sha256`、`size`；普通文件与符号链接的 `sha256` 都对内容字节计算，符号链接的内容
//! 是其链接目标的 UTF-8 字节。普通目录使用独立摘要域 `mai.reference.directory_tree.v1`
//! （字段 `kind`、`mode`、`path`、`sha256`、`size`，没有 Git 对象 ID），两者绝不混用。
//!
//! 摘要不是签名：它只能证明"这份内容与这个摘要一致"，Git 归属仍依赖可信发布流程。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use mai_protocol::{ReferenceErrorCode, SourceProvenance};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::canonical::{field, quote};
use crate::error::{ReferenceError, Result};
use crate::tree::{SourceEntryKind, SourceTreeEntry, SourceTreeScan};

/// 发布器写在版本目录里的元数据文件名；它与被挂载的 `repo/` 平级。
pub const SOURCE_MANIFEST_FILE_NAME: &str = "source-manifest.json";

/// 发布器 manifest 的格式标记。
pub const SOURCE_MANIFEST_FORMAT: &str = "mai.reference.source.v1";

/// Git 快照内容摘要的格式标记。
pub const GIT_TREE_DIGEST_FORMAT: &str = "mai.reference.tree.v1";

/// 普通目录内容摘要的格式标记。
pub const DIRECTORY_TREE_DIGEST_FORMAT: &str = "mai.reference.directory_tree.v1";

/// manifest 中 `provenance.kind` 唯一接受的取值。
pub const GIT_TREE_PROVENANCE_KIND: &str = "git_tree";

/// Git 对象的模式：普通文件、可执行文件、符号链接。
const GIT_MODE_FILE: &str = "100644";
const GIT_MODE_EXECUTABLE: &str = "100755";
const GIT_MODE_SYMLINK: &str = "120000";

/// manifest 里的单个条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceManifestEntry {
    pub path: String,
    pub git_mode: String,
    pub git_blob: String,
    pub kind: SourceEntryKind,
    pub size: u64,
    pub sha256: String,
}

/// manifest 里的发布来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceManifestProvenance {
    pub kind: String,
    pub commit_sha: String,
    pub tree_sha: String,
}

/// 发布器写出的 `source-manifest.json` 文档。
///
/// 未知字段一律拒绝：manifest 是被挂载内容的身份依据，静默忽略字段会让来源声明
/// 与核对结果脱节。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceManifest {
    pub format: String,
    pub provenance: SourceManifestProvenance,
    pub manifest_digest: String,
    pub file_count: u64,
    pub total_bytes: u64,
    pub entries: Vec<SourceManifestEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 通过逐项核对的 manifest 结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedManifest {
    provenance: SourceProvenance,
    manifest_digest: String,
    entry_count: u64,
    byte_count: u64,
}

impl VerifiedManifest {
    /// 已核实的发布来源。
    pub fn provenance(&self) -> &SourceProvenance {
        &self.provenance
    }

    /// 已复算一致的规范摘要（`sha256:` 前缀）。
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }

    /// 条目数（文件与符号链接，目录不计）。
    pub fn entry_count(&self) -> u64 {
        self.entry_count
    }

    /// 条目字节总量（与 manifest 的 `total_bytes` 同义）。
    pub fn byte_count(&self) -> u64 {
        self.byte_count
    }
}

/// 定位版本目录中的 manifest：登记路径（`.../<commit>/repo`）的父目录下的
/// `source-manifest.json`。符号链接或非普通文件都不算 manifest。
pub fn locate_manifest(host_path: &Path) -> Option<PathBuf> {
    let parent = host_path.parent()?;
    let candidate = parent.join(SOURCE_MANIFEST_FILE_NAME);
    let metadata = fs::symlink_metadata(&candidate).ok()?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() || !file_type.is_file() {
        return None;
    }
    Some(candidate)
}

/// 读取并严格解析 manifest；JSON 结构不合法时返回 `manifest_invalid`。
pub fn load_manifest(path: &Path) -> Result<SourceManifest> {
    let bytes = fs::read(path).map_err(|error| {
        ReferenceError::from_io(
            ReferenceErrorCode::SourceUnreadable,
            format!("无法读取发布 manifest：{}", path.display()),
            error,
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        ReferenceError::new(
            ReferenceErrorCode::ManifestInvalid,
            format!("发布 manifest 结构不符合契约：{}", path.display()),
        )
        .with_cause(error)
    })
}

/// 读取登记路径对应的 manifest；没有 manifest 时返回 `None`（普通目录快照）。
pub fn read_source_manifest(host_path: &Path) -> Result<Option<SourceManifest>> {
    match locate_manifest(host_path) {
        None => Ok(None),
        Some(path) => load_manifest(&path).map(Some),
    }
}

/// 与磁盘实测结果逐项核对 manifest：格式、对象 ID 语法、路径安全、摘要复算、
/// 条目集合完全一致（既不能缺，也不能多），并核对发布方声明的计数与字节量。
pub fn verify_manifest(
    manifest: &SourceManifest,
    scan: &SourceTreeScan,
) -> Result<VerifiedManifest> {
    if manifest.format != SOURCE_MANIFEST_FORMAT {
        return Err(ReferenceError::new(
            ReferenceErrorCode::UnsupportedManifestFormat,
            format!("不支持的发布 manifest 格式：{}", manifest.format),
        ));
    }
    if manifest.provenance.kind != GIT_TREE_PROVENANCE_KIND {
        return Err(ReferenceError::new(
            ReferenceErrorCode::UnsupportedManifestFormat,
            format!(
                "不支持的发布来源种类：{}；普通目录不应携带 manifest",
                manifest.provenance.kind
            ),
        ));
    }
    validate_object_id(&manifest.provenance.commit_sha, "provenance.commit_sha")?;
    validate_object_id(&manifest.provenance.tree_sha, "provenance.tree_sha")?;

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut declared_bytes: u64 = 0;
    for entry in &manifest.entries {
        validate_manifest_path(&entry.path)?;
        if !seen.insert(entry.path.as_str()) {
            return Err(ReferenceError::new(
                ReferenceErrorCode::ManifestInvalid,
                format!("发布 manifest 出现重复路径：{}", entry.path),
            ));
        }
        match entry.kind {
            SourceEntryKind::File | SourceEntryKind::Symlink => {}
            SourceEntryKind::Directory => {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::ManifestInvalid,
                    format!("发布 manifest 不应包含目录条目：{}", entry.path),
                ));
            }
        }
        validate_git_mode(&entry.kind, &entry.git_mode, &entry.path)?;
        validate_object_id(
            &entry.git_blob,
            &format!("entries[{0}].git_blob", entry.path),
        )?;
        validate_sha256(&entry.sha256, &entry.path)?;
        declared_bytes = declared_bytes.checked_add(entry.size).ok_or_else(|| {
            ReferenceError::new(
                ReferenceErrorCode::ManifestInvalid,
                "发布 manifest 的字节总量溢出",
            )
        })?;
    }

    if manifest.file_count != manifest.entries.len() as u64 {
        return Err(ReferenceError::new(
            ReferenceErrorCode::ManifestInvalid,
            format!(
                "发布 manifest 条目数与 file_count 不一致：{} / {}",
                manifest.entries.len(),
                manifest.file_count
            ),
        ));
    }
    if manifest.total_bytes != declared_bytes {
        return Err(ReferenceError::new(
            ReferenceErrorCode::ManifestInvalid,
            format!(
                "发布 manifest 字节量与 total_bytes 不一致：{} / {}",
                declared_bytes, manifest.total_bytes
            ),
        ));
    }

    let recomputed = tree_digest_from_entries(&manifest.entries);
    if recomputed != manifest.manifest_digest {
        return Err(ReferenceError::new(
            ReferenceErrorCode::ManifestDigestMismatch,
            format!(
                "发布 manifest 摘要与条目复算结果不一致：声明 {}，复算 {}",
                manifest.manifest_digest, recomputed
            ),
        ));
    }

    if manifest.file_count != scan.entry_count() {
        return Err(ReferenceError::new(
            ReferenceErrorCode::ManifestEntryMismatch,
            format!(
                "发布 manifest 条目数与磁盘实际不一致：{} / {}",
                manifest.file_count,
                scan.entry_count()
            ),
        ));
    }
    if manifest.total_bytes != scan.byte_count() {
        return Err(ReferenceError::new(
            ReferenceErrorCode::ManifestEntryMismatch,
            format!(
                "发布 manifest 字节量与磁盘实际不一致：{} / {}",
                manifest.total_bytes,
                scan.byte_count()
            ),
        ));
    }

    for entry in &manifest.entries {
        let actual = scan.find(&entry.path).ok_or_else(|| {
            ReferenceError::new(
                ReferenceErrorCode::ManifestEntryMismatch,
                format!("发布 manifest 声明的路径在磁盘上不存在：{}", entry.path),
            )
        })?;
        if actual.kind != entry.kind {
            return Err(ReferenceError::new(
                ReferenceErrorCode::ManifestEntryMismatch,
                format!("条目类型与 manifest 不一致：{}", entry.path),
            ));
        }
        if actual.size != entry.size {
            return Err(ReferenceError::new(
                ReferenceErrorCode::ManifestEntryMismatch,
                format!("条目字节数与 manifest 不一致：{}", entry.path),
            ));
        }
        if actual.sha256 != entry.sha256 {
            return Err(ReferenceError::new(
                ReferenceErrorCode::ManifestEntryMismatch,
                format!("条目内容摘要与 manifest 不一致：{}", entry.path),
            ));
        }
        if actual.git_blob.as_deref() != Some(entry.git_blob.as_str()) {
            return Err(ReferenceError::new(
                ReferenceErrorCode::ManifestEntryMismatch,
                format!("条目 Git 对象 ID 与 manifest 不一致：{}", entry.path),
            ));
        }
        if actual.executable != (entry.git_mode == GIT_MODE_EXECUTABLE) {
            return Err(ReferenceError::new(
                ReferenceErrorCode::ManifestEntryMismatch,
                format!("条目执行位与 manifest 的 git_mode 不一致：{}", entry.path),
            ));
        }
    }

    for actual in scan.entries() {
        if actual.kind == SourceEntryKind::Directory {
            continue;
        }
        if !seen.contains(actual.path.as_str()) {
            return Err(ReferenceError::new(
                ReferenceErrorCode::ManifestEntryMismatch,
                format!("磁盘上存在 manifest 未声明的条目：{}", actual.path),
            ));
        }
    }

    Ok(VerifiedManifest {
        provenance: SourceProvenance::git_tree(
            manifest.provenance.commit_sha.clone(),
            manifest.provenance.tree_sha.clone(),
        ),
        manifest_digest: manifest.manifest_digest.clone(),
        entry_count: manifest.file_count,
        byte_count: manifest.total_bytes,
    })
}

/// 复算 Git 快照摘要。条目先按路径字典序规范化，因此 manifest 文件里的排列顺序
/// 不影响复算结果。
pub fn tree_digest_from_entries(entries: &[SourceManifestEntry]) -> String {
    let mut sorted: Vec<&SourceManifestEntry> = entries.iter().collect();
    sorted.sort_by(|left, right| left.path.cmp(&right.path));

    let documents: Vec<String> = sorted.into_iter().map(git_entry_document).collect();
    digest_document(GIT_TREE_DIGEST_FORMAT, &documents)
}

/// 复算普通目录摘要；目录条目不参与内容身份。
pub fn directory_tree_digest(entries: &[SourceTreeEntry]) -> String {
    let mut sorted: Vec<&SourceTreeEntry> = entries
        .iter()
        .filter(|entry| entry.kind != SourceEntryKind::Directory)
        .collect();
    sorted.sort_by(|left, right| left.path.cmp(&right.path));

    let documents: Vec<String> = sorted.into_iter().map(directory_entry_document).collect();
    digest_document(DIRECTORY_TREE_DIGEST_FORMAT, &documents)
}

/// Git 快照条目文档，键按字典序：`git_blob`、`git_mode`、`kind`、`path`、`sha256`、`size`。
fn git_entry_document(entry: &SourceManifestEntry) -> String {
    let mut out = String::new();
    out.push('{');
    out.push_str(&field("git_blob", &quote(&entry.git_blob)));
    out.push(',');
    out.push_str(&field("git_mode", &quote(&entry.git_mode)));
    out.push(',');
    out.push_str(&field("kind", &quote(entry.kind.as_str())));
    out.push(',');
    out.push_str(&field("path", &quote(&entry.path)));
    out.push(',');
    out.push_str(&field("sha256", &quote(&entry.sha256)));
    out.push(',');
    out.push_str(&field("size", &entry.size.to_string()));
    out.push('}');
    out
}

/// 普通目录条目文档，键按字典序：`kind`、`mode`、`path`、`sha256`、`size`。
fn directory_entry_document(entry: &SourceTreeEntry) -> String {
    let mut out = String::new();
    out.push('{');
    out.push_str(&field("kind", &quote(entry.kind.as_str())));
    out.push(',');
    out.push_str(&field("mode", &quote(directory_mode(entry))));
    out.push(',');
    out.push_str(&field("path", &quote(&entry.path)));
    out.push(',');
    out.push_str(&field("sha256", &quote(&entry.sha256)));
    out.push(',');
    out.push_str(&field("size", &entry.size.to_string()));
    out.push('}');
    out
}

/// 普通目录里条目的模式：可执行文件 100755，普通文件 100644，符号链接 120000。
fn directory_mode(entry: &SourceTreeEntry) -> &'static str {
    match entry.kind {
        SourceEntryKind::Symlink => GIT_MODE_SYMLINK,
        SourceEntryKind::File if entry.executable => GIT_MODE_EXECUTABLE,
        SourceEntryKind::File => GIT_MODE_FILE,
        SourceEntryKind::Directory => GIT_MODE_FILE,
    }
}

/// 拼出最终摘要文档 `{"entries":[...],"format":"..."}` 并加 `sha256:` 前缀。
fn digest_document(format: &str, entries: &[String]) -> String {
    let mut out = String::with_capacity(entries.iter().map(String::len).sum::<usize>() + 64);
    out.push_str("{\"entries\":[");
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(entry);
    }
    out.push_str("],");
    out.push_str(&field("format", &quote(format)));
    out.push('}');

    let mut hasher = sha2::Sha256::new();
    hasher.update(out.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

fn validate_manifest_path(path: &str) -> Result<()> {
    validate_relative_path(path).map_err(|message| {
        ReferenceError::new(
            ReferenceErrorCode::ManifestInvalid,
            format!("发布 manifest 条目路径不安全：{path}（{message}）"),
        )
    })
}

/// 相对路径校验：非空、无绝对前缀、无空组件、无 `.`/`..`/`.git` 组件、无控制字符，
/// 且等于自身归一化结果。
pub(crate) fn validate_relative_path(path: &str) -> std::result::Result<(), &'static str> {
    if path.is_empty() {
        return Err("路径为空");
    }
    if path.starts_with('/') || path.ends_with('/') {
        return Err("路径必须是相对路径且不能以分隔符结尾");
    }
    if path.chars().any(|character| {
        let code = character as u32;
        code < 0x20 || code == 0x7f
    }) {
        return Err("路径包含控制字符");
    }
    for component in path.split('/') {
        match component {
            "" => return Err("路径包含空组件"),
            "." | ".." => return Err("路径包含相对组件"),
            ".git" => return Err("路径包含 .git 组件"),
            _ => {}
        }
    }
    Ok(())
}

fn validate_git_mode(kind: &SourceEntryKind, mode: &str, path: &str) -> Result<()> {
    let expected: &[&str] = match kind {
        SourceEntryKind::File => &[GIT_MODE_FILE, GIT_MODE_EXECUTABLE],
        SourceEntryKind::Symlink => &[GIT_MODE_SYMLINK],
        SourceEntryKind::Directory => &[],
    };
    if !expected.contains(&mode) {
        return Err(ReferenceError::new(
            ReferenceErrorCode::ManifestInvalid,
            format!("发布 manifest 的 git_mode 与条目类型不匹配：{path} -> {mode}"),
        ));
    }
    Ok(())
}

fn validate_object_id(value: &str, label: &str) -> Result<()> {
    let length = value.len();
    let valid = (length == 40 || length == 64) && value.chars().all(is_lower_hex_digit);
    if !valid {
        return Err(ReferenceError::new(
            ReferenceErrorCode::ManifestInvalid,
            format!("发布 manifest 的 {label} 不是完整十六进制对象 ID"),
        ));
    }
    Ok(())
}

fn validate_sha256(value: &str, path: &str) -> Result<()> {
    let valid = value.len() == 64 && value.chars().all(is_lower_hex_digit);
    if !valid {
        return Err(ReferenceError::new(
            ReferenceErrorCode::ManifestInvalid,
            format!("发布 manifest 的 sha256 不是 64 位小写十六进制：{path}"),
        ));
    }
    Ok(())
}

/// 判断字符是否是小写十六进制数字。
pub(crate) fn is_lower_hex_digit(character: char) -> bool {
    character.is_ascii_digit() || ('a'..='f').contains(&character)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{ReferenceDeploymentPolicy, SourceTrustRequirement, validate_source_root};
    use crate::tree::scan_source_tree;
    use pretty_assertions::assert_eq;

    fn policy() -> ReferenceDeploymentPolicy {
        ReferenceDeploymentPolicy::new(vec![std::env::temp_dir()])
            .expect("允许根")
            .with_source_trust_requirement(SourceTrustRequirement::Unrestricted)
    }

    fn scan_fixture(root: &Path) -> SourceTreeScan {
        let validated = validate_source_root(&policy(), &root.to_string_lossy()).expect("源根");
        scan_source_tree(&validated, &policy()).expect("扫描")
    }

    #[test]
    fn directory_digest_is_stable_and_domain_separated() {
        let entries = vec![
            SourceTreeEntry {
                path: "b.c".to_string(),
                kind: SourceEntryKind::File,
                size: 3,
                sha256: "b".repeat(64),
                git_blob: Some("c".repeat(40)),
                executable: false,
                link_target: None,
            },
            SourceTreeEntry {
                path: "a.c".to_string(),
                kind: SourceEntryKind::File,
                size: 1,
                sha256: "a".repeat(64),
                git_blob: Some("d".repeat(40)),
                executable: true,
                link_target: None,
            },
        ];
        let first = directory_tree_digest(&entries);
        let second = directory_tree_digest(&entries.iter().rev().cloned().collect::<Vec<_>>());
        assert_eq!(first, second);
        assert!(first.starts_with("sha256:"));

        let manifest_entries = vec![SourceManifestEntry {
            path: "a.c".to_string(),
            git_mode: GIT_MODE_FILE.to_string(),
            git_blob: "d".repeat(40),
            kind: SourceEntryKind::File,
            size: 1,
            sha256: "a".repeat(64),
        }];
        assert_ne!(first, tree_digest_from_entries(&manifest_entries));
    }

    #[test]
    fn manifest_and_scan_round_trip() {
        let temp = tempfile::tempdir().expect("临时目录");
        let version = temp.path().join("version");
        let root = version.join("repo");
        std::fs::create_dir_all(root.join("drivers")).expect("目录");
        std::fs::write(
            root.join("drivers/foo.c"),
            b"int fixture(void) { return 1; }\n",
        )
        .expect("文件");
        std::fs::write(root.join("README"), b"fixture\n").expect("文件");

        let scan = scan_fixture(&root);
        assert_eq!(2, scan.entry_count());
        assert_eq!(2, scan.file_count());
        assert_eq!(0, scan.symlink_count());

        let entries = scan
            .entries()
            .iter()
            .filter(|entry| entry.kind != SourceEntryKind::Directory)
            .map(|entry| SourceManifestEntry {
                path: entry.path.clone(),
                git_mode: if entry.executable {
                    GIT_MODE_EXECUTABLE.to_string()
                } else {
                    GIT_MODE_FILE.to_string()
                },
                git_blob: entry.git_blob.clone().expect("Git 对象 ID"),
                kind: entry.kind,
                size: entry.size,
                sha256: entry.sha256.clone(),
            })
            .collect::<Vec<_>>();

        let manifest = SourceManifest {
            format: SOURCE_MANIFEST_FORMAT.to_string(),
            provenance: SourceManifestProvenance {
                kind: GIT_TREE_PROVENANCE_KIND.to_string(),
                commit_sha: "f635d71a4fc1f3822a107295e98ca717c0f724fa".to_string(),
                tree_sha: "9e56ed9f6d1b0a4ea2d2cf1c3f1c5f9b1d3a2c40".to_string(),
            },
            manifest_digest: tree_digest_from_entries(&entries),
            file_count: entries.len() as u64,
            total_bytes: scan.byte_count(),
            entries: entries.clone(),
            note: None,
        };

        let verified = verify_manifest(&manifest, &scan).expect("manifest 核对");
        assert_eq!(manifest.manifest_digest, verified.manifest_digest());
        assert_eq!(2, verified.entry_count());
        assert_eq!(
            Some(manifest.provenance.tree_sha.clone()),
            verified.provenance().tree_sha.clone()
        );

        let mut tampered = manifest.clone();
        tampered.entries[0].sha256 = "0".repeat(64);
        tampered.manifest_digest = tree_digest_from_entries(&tampered.entries);
        let error = verify_manifest(&tampered, &scan).expect_err("内容被篡改必须失败");
        assert_eq!(ReferenceErrorCode::ManifestEntryMismatch, error.code());
    }

    #[test]
    fn extra_file_on_disk_is_rejected() {
        let temp = tempfile::tempdir().expect("临时目录");
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).expect("目录");
        std::fs::write(root.join("a.c"), b"a\n").expect("文件");
        let scan = scan_fixture(&root);

        let manifest = SourceManifest {
            format: SOURCE_MANIFEST_FORMAT.to_string(),
            provenance: SourceManifestProvenance {
                kind: GIT_TREE_PROVENANCE_KIND.to_string(),
                commit_sha: "a".repeat(40),
                tree_sha: "b".repeat(40),
            },
            manifest_digest: tree_digest_from_entries(&[]),
            file_count: 0,
            total_bytes: 0,
            entries: Vec::new(),
            note: Some("fixture".to_string()),
        };

        let error = verify_manifest(&manifest, &scan).expect_err("多余文件必须失败");
        assert_eq!(ReferenceErrorCode::ManifestEntryMismatch, error.code());
    }

    #[test]
    fn manifest_paths_with_relative_components_are_rejected() {
        assert_eq!(Err("路径包含相对组件"), validate_relative_path("../escape"));
        assert_eq!(
            Err("路径包含 .git 组件"),
            validate_relative_path(".git/config")
        );
        assert_eq!(
            Err("路径必须是相对路径且不能以分隔符结尾"),
            validate_relative_path("/etc")
        );
        assert_eq!(Ok(()), validate_relative_path("drivers/irqchip/foo.c"));
    }

    #[test]
    fn manifest_rejects_wrong_format_and_provenance_kind() {
        let temp = tempfile::tempdir().expect("临时目录");
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).expect("目录");
        std::fs::write(root.join("a.c"), b"a\n").expect("文件");
        let scan = scan_fixture(&root);

        let mut manifest = SourceManifest {
            format: "mai.reference.tree.v1".to_string(),
            provenance: SourceManifestProvenance {
                kind: "directory_tree".to_string(),
                commit_sha: "a".repeat(40),
                tree_sha: "b".repeat(40),
            },
            manifest_digest: tree_digest_from_entries(&[]),
            file_count: 0,
            total_bytes: 0,
            entries: Vec::new(),
            note: None,
        };
        assert_eq!(
            ReferenceErrorCode::UnsupportedManifestFormat,
            verify_manifest(&manifest, &scan)
                .expect_err("格式错")
                .code()
        );

        manifest.format = SOURCE_MANIFEST_FORMAT.to_string();
        assert_eq!(
            ReferenceErrorCode::UnsupportedManifestFormat,
            verify_manifest(&manifest, &scan)
                .expect_err("来源种类错")
                .code()
        );
    }

    #[test]
    fn locate_manifest_only_accepts_regular_sibling_file() {
        let temp = tempfile::tempdir().expect("临时目录");
        let version = temp.path().join("version");
        let root = version.join("repo");
        std::fs::create_dir_all(&root).expect("目录");
        assert_eq!(None, locate_manifest(&root));

        std::fs::write(version.join(SOURCE_MANIFEST_FILE_NAME), b"{}").expect("manifest");
        assert_eq!(
            Some(version.join(SOURCE_MANIFEST_FILE_NAME)),
            locate_manifest(&root)
        );
        // 存在但结构不合法的 manifest 必须显式失败，不能退化成"没有 manifest"。
        assert_eq!(
            ReferenceErrorCode::ManifestInvalid,
            read_source_manifest(&root)
                .expect_err("非法 manifest")
                .code()
        );
    }
}
