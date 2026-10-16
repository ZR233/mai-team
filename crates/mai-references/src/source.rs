//! 参考源登记、完整验证与重新验证。
//!
//! 登记流程：HTTP 层先做 O(1) 语法/鉴权检查，之后由受控 worker 调用这里的验证。
//! 验证通过才把记录置为 `Ready`；失败必须保留精确的失败原因，不能把"没有 manifest"
//! 与"manifest 与内容不一致"混为一谈。

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use mai_protocol::{
    MAX_REVISION_LABEL_CHARS, MAX_SOURCE_NAME_CHARS, NewReferenceSourceRequest, ReferenceErrorCode,
    ReferenceSource, ReferenceValidationFailure, SourceProvenance, SourceProvenanceKind,
    SourceState,
};
use uuid::Uuid;

use crate::error::{ReferenceError, Result};
use crate::manifest::{read_source_manifest, verify_manifest};
use crate::policy::{ReferenceDeploymentPolicy, ValidatedSourceRoot, validate_source_root};
use crate::tree::scan_source_tree;

// 约定：Git 发布目录通常命名为 `<commit-sha>`，登记路径是它的 `repo/` 子目录。
// 目录名只作为佐证记录，不把"目录名等于 commit"当成内容真实性的证明。

/// 参考源在"能否被项目启用"意义上的可用性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceAvailability {
    /// 验证通过且管理员已确认可供 Agent/模型访问。
    Usable,
    /// 仍在验证中，尚不能被启用。
    AwaitingValidation,
    /// 验证通过但缺少"已确认适合提供给 Agent/模型"的确认项。
    NotApproved,
    /// 目录缺失、身份失效或被全局禁用。
    Unavailable,
}

impl SourceAvailability {
    /// 是否可以被项目绑定启用。
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Usable)
    }

    /// 由登记记录派生可用性。
    pub fn of(source: &ReferenceSource) -> Self {
        match source.state {
            SourceState::Validating => Self::AwaitingValidation,
            SourceState::Ready if source.approved_for_agent_access => Self::Usable,
            SourceState::Ready => Self::NotApproved,
            SourceState::Invalid | SourceState::Disabled | SourceState::Missing => {
                Self::Unavailable
            }
        }
    }

    /// 中文说明，用于错误消息。
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Usable => "可用",
            Self::AwaitingValidation => "仍在验证中",
            Self::NotApproved => "尚未确认可供 Agent/模型访问",
            Self::Unavailable => "不可用（目录缺失、身份失效或已被禁用）",
        }
    }
}

/// 登记身份：登记后不可修改的字段组合。
///
/// `source_id + canonical_path + provenance + manifest_digest + dev/ino` 一旦确定，
/// 只能通过新登记来表达内容变化；PATCH 不能改动其中任何一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceIdentity {
    canonical_path: PathBuf,
    manifest_digest: String,
    provenance: SourceProvenance,
    device: u64,
    inode: u64,
}

impl SourceIdentity {
    /// 规范宿主路径。
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    /// 内容摘要（`sha256:` 前缀）。
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }

    /// 发布来源。
    pub fn provenance(&self) -> &SourceProvenance {
        &self.provenance
    }

    /// 目录所在设备号。
    pub fn device(&self) -> u64 {
        self.device
    }

    /// 目录 inode。
    pub fn inode(&self) -> u64 {
        self.inode
    }
}

/// 通过完整验证的参考源；只能由本模块构造。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedReferenceSource {
    identity: SourceIdentity,
    allowed_root: PathBuf,
    owner_uid: u32,
    file_count: u64,
    directory_count: u64,
    symlink_count: u64,
    entry_count: u64,
    byte_count: u64,
    warnings: Vec<String>,
    verified_at: DateTime<Utc>,
}

impl ValidatedReferenceSource {
    /// 规范宿主路径。
    pub fn host_path(&self) -> &Path {
        self.identity.canonical_path()
    }

    /// 命中的部署允许根。
    pub fn allowed_root(&self) -> &Path {
        &self.allowed_root
    }

    /// 登记身份。
    pub fn identity(&self) -> &SourceIdentity {
        &self.identity
    }

    /// 内容摘要（`sha256:` 前缀）。
    pub fn manifest_digest(&self) -> &str {
        self.identity.manifest_digest()
    }

    /// 发布来源。
    pub fn provenance(&self) -> &SourceProvenance {
        self.identity.provenance()
    }

    /// 目录持有者 UID。
    pub fn owner_uid(&self) -> u32 {
        self.owner_uid
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

    /// 参与内容身份的条目数（文件 + 符号链接）。
    pub fn entry_count(&self) -> u64 {
        self.entry_count
    }

    /// 条目字节总量。
    pub fn byte_count(&self) -> u64 {
        self.byte_count
    }

    /// 不阻塞登记、但必须显示给管理员的告警。
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// 本次验证时间。
    pub fn verified_at(&self) -> DateTime<Utc> {
        self.verified_at
    }
}

/// Git 来源的目录名佐证结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvenanceCorroboration {
    /// 普通目录快照，或目录名与 manifest 的 commit 一致。
    Corroborated,
    /// 目录名不是 manifest 声明的 commit。
    DirectoryNameMismatch {
        directory_name: String,
        commit_sha: String,
    },
    /// `git_tree` 来源缺少 commit 标识（只可能来自被篡改的记录）。
    MissingCommitSha,
}

/// 登记请求的校验与完整验证。
pub fn validate_new_source(
    policy: &ReferenceDeploymentPolicy,
    request: &NewReferenceSourceRequest,
    verified_at: DateTime<Utc>,
) -> Result<ValidatedReferenceSource> {
    validate_source_name(&request.name)?;
    validate_revision_label(&request.revision_label)?;
    verify_source(policy, &request.host_path, verified_at)
}

/// 重新验证已有登记项：先重新完成全部内容验证，再比对登记身份。
///
/// 身份任意一项变化都返回 `source_identity_changed`：外部原地修改已登记快照不会
/// 被静默接受，必须由管理员重新发布并新登记。
pub fn revalidate_source(
    policy: &ReferenceDeploymentPolicy,
    record: &ReferenceSource,
    verified_at: DateTime<Utc>,
) -> Result<ValidatedReferenceSource> {
    let validated = verify_source(policy, &record.host_path, verified_at)?;
    let identity = validated.identity();

    if identity.canonical_path().to_string_lossy() != record.host_path.as_str() {
        return Err(identity_changed(format!(
            "登记路径已变化：{} -> {}",
            record.host_path,
            identity.canonical_path().display()
        )));
    }
    if record.manifest_digest.as_deref() != Some(identity.manifest_digest()) {
        return Err(identity_changed(format!(
            "内容摘要已变化：{:?} -> {}",
            record.manifest_digest,
            identity.manifest_digest()
        )));
    }
    if record.provenance != *identity.provenance() {
        return Err(identity_changed("发布来源已变化"));
    }
    if record.source_device != identity.device() || record.source_inode != identity.inode() {
        return Err(identity_changed(format!(
            "目录身份已变化：dev/ino {}:{} -> {}:{}",
            record.source_device,
            record.source_inode,
            identity.device(),
            identity.inode()
        )));
    }
    Ok(validated)
}

/// 生成 202 阶段的 `Validating` 记录。
///
/// 验证完成前无法确定发布来源，因此 `provenance` 只是占位；验证通过后由
/// [`ready_source_record`] 用真实结论覆盖。
pub fn validating_source_record(
    id: Uuid,
    request: &NewReferenceSourceRequest,
    created_at: DateTime<Utc>,
) -> Result<ReferenceSource> {
    validate_source_name(&request.name)?;
    validate_revision_label(&request.revision_label)?;

    Ok(ReferenceSource {
        id,
        name: request.name.clone(),
        revision_label: request.revision_label.clone(),
        host_path: request.host_path.clone(),
        provenance: SourceProvenance::directory_snapshot(),
        manifest_digest: None,
        state: SourceState::Validating,
        approved_for_agent_access: request.approved_for_agent_access,
        file_count: 0,
        byte_count: 0,
        source_device: 0,
        source_inode: 0,
        validation_error: None,
        warnings: Vec::new(),
        created_at,
        updated_at: created_at,
    })
}

/// 生成验证通过后的 `Ready` 记录。
pub fn ready_source_record(
    id: Uuid,
    request: &NewReferenceSourceRequest,
    validated: &ValidatedReferenceSource,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
) -> ReferenceSource {
    ReferenceSource {
        id,
        name: request.name.clone(),
        revision_label: request.revision_label.clone(),
        host_path: validated.host_path().to_string_lossy().into_owned(),
        provenance: validated.provenance().clone(),
        manifest_digest: Some(validated.manifest_digest().to_string()),
        state: SourceState::Ready,
        approved_for_agent_access: request.approved_for_agent_access,
        file_count: validated.entry_count(),
        byte_count: validated.byte_count(),
        source_device: validated.identity().device(),
        source_inode: validated.identity().inode(),
        validation_error: None,
        warnings: validated.warnings().to_vec(),
        created_at,
        updated_at,
    }
}

/// 把领域错误转成可持久化的验证失败记录。
pub fn validation_failure(
    error: &ReferenceError,
    observed_at: DateTime<Utc>,
) -> ReferenceValidationFailure {
    ReferenceValidationFailure {
        code: error.code(),
        message: error.message().to_string(),
        path: None,
        observed_at,
    }
}

/// 判断 manifest 声明的 Git 来源是否与发布目录名一致。
pub fn provenance_corroboration(
    host_path: &Path,
    provenance: &SourceProvenance,
) -> ProvenanceCorroboration {
    if provenance.kind != SourceProvenanceKind::GitTree {
        return ProvenanceCorroboration::Corroborated;
    }
    let directory_name = host_path
        .parent()
        .and_then(|parent| parent.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    match provenance.commit_sha.as_deref() {
        Some(commit_sha) if commit_sha == directory_name => ProvenanceCorroboration::Corroborated,
        Some(commit_sha) => ProvenanceCorroboration::DirectoryNameMismatch {
            directory_name,
            commit_sha: commit_sha.to_string(),
        },
        None => ProvenanceCorroboration::MissingCommitSha,
    }
}

fn verify_source(
    policy: &ReferenceDeploymentPolicy,
    host_path: &str,
    verified_at: DateTime<Utc>,
) -> Result<ValidatedReferenceSource> {
    let root: ValidatedSourceRoot = validate_source_root(policy, host_path)?;
    let scan = scan_source_tree(&root, policy)?;

    let (provenance, manifest_digest, entry_count, byte_count, warnings) =
        match read_source_manifest(root.path())? {
            Some(manifest) => {
                let verified = verify_manifest(&manifest, &scan)?;
                let provenance = verified.provenance().clone();
                let mut warnings = Vec::new();
                match provenance_corroboration(root.path(), &provenance) {
                    ProvenanceCorroboration::Corroborated => {}
                    ProvenanceCorroboration::DirectoryNameMismatch {
                        directory_name,
                        commit_sha,
                    } => warnings.push(format!(
                        "发布目录名 {directory_name} 与 manifest 声明的 commit {commit_sha} 不一致：来源声明只反映可信发布记录，本服务未核实该目录等同于该 commit"
                    )),
                    ProvenanceCorroboration::MissingCommitSha => warnings.push(
                        "manifest 声明 Git 来源但缺少 commit 标识：请重新发布该快照".to_string(),
                    ),
                }
                (
                    provenance,
                    verified.manifest_digest().to_string(),
                    verified.entry_count(),
                    verified.byte_count(),
                    warnings,
                )
            }
            None => (
                SourceProvenance::directory_snapshot(),
                scan.digest().to_string(),
                scan.entry_count(),
                scan.byte_count(),
                vec![
                    "该目录没有发布 manifest，按目录快照登记；不声称对应任何 Git commit"
                        .to_string(),
                ],
            ),
        };

    if entry_count == 0 {
        return Err(ReferenceError::new(
            ReferenceErrorCode::SourceEmpty,
            "参考源没有任何文件，拒绝登记空参考源",
        ));
    }

    let allowed_root = root.allowed_root().to_path_buf();
    let owner_uid = root.owner_uid();

    Ok(ValidatedReferenceSource {
        identity: SourceIdentity {
            canonical_path: root.path().to_path_buf(),
            manifest_digest,
            provenance,
            device: root.device(),
            inode: root.inode(),
        },
        allowed_root,
        owner_uid,
        file_count: scan.file_count(),
        directory_count: scan.directory_count(),
        symlink_count: scan.symlink_count(),
        entry_count,
        byte_count,
        warnings,
        verified_at,
    })
}

fn validate_source_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(ReferenceError::new(
            ReferenceErrorCode::InvalidSourceName,
            "参考源名称不能为空",
        )
        .with_field("name"));
    }
    if name.chars().count() > MAX_SOURCE_NAME_CHARS {
        return Err(ReferenceError::new(
            ReferenceErrorCode::InvalidSourceName,
            format!("参考源名称不能超过 {MAX_SOURCE_NAME_CHARS} 个字符"),
        )
        .with_field("name"));
    }
    if contains_control_character(name) {
        return Err(ReferenceError::new(
            ReferenceErrorCode::InvalidSourceName,
            "参考源名称不能包含控制字符",
        )
        .with_field("name"));
    }
    Ok(())
}

fn validate_revision_label(label: &str) -> Result<()> {
    if label.chars().count() > MAX_REVISION_LABEL_CHARS {
        return Err(ReferenceError::new(
            ReferenceErrorCode::InvalidRevisionLabel,
            format!("版本标签不能超过 {MAX_REVISION_LABEL_CHARS} 个字符"),
        )
        .with_field("revision_label"));
    }
    if contains_control_character(label) {
        return Err(ReferenceError::new(
            ReferenceErrorCode::InvalidRevisionLabel,
            "版本标签不能包含控制字符",
        )
        .with_field("revision_label"));
    }
    Ok(())
}

fn contains_control_character(value: &str) -> bool {
    value
        .chars()
        .any(|character| (character as u32) < 0x20 || (character as u32) == 0x7f)
}

fn identity_changed(message: impl Into<String>) -> ReferenceError {
    ReferenceError::new(ReferenceErrorCode::SourceIdentityChanged, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        GIT_TREE_PROVENANCE_KIND, SOURCE_MANIFEST_FILE_NAME, SOURCE_MANIFEST_FORMAT,
        SourceManifest, SourceManifestEntry, SourceManifestProvenance, tree_digest_from_entries,
    };
    use crate::policy::{ReferenceDeploymentPolicy, SourceTrustRequirement, validate_source_root};
    use crate::tree::{SourceEntryKind, scan_source_tree};
    use pretty_assertions::assert_eq;

    struct Fixture {
        _temp: tempfile::TempDir,
        version: PathBuf,
        root: PathBuf,
        commit_sha: String,
    }

    fn make_fixture(files: &[(&str, &[u8])]) -> Fixture {
        let temp = tempfile::tempdir().expect("临时目录");
        let commit_sha = "f635d71a4fc1f3822a107295e98ca717c0f724fa".to_string();
        let version = temp.path().join(&commit_sha);
        let root = version.join("repo");
        std::fs::create_dir_all(&root).expect("源码根");
        for (name, content) in files {
            let path = root.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("父目录");
            }
            std::fs::write(path, content).expect("文件");
        }
        Fixture {
            _temp: temp,
            version,
            root,
            commit_sha,
        }
    }

    fn fixture_policy(fixture: &Fixture) -> ReferenceDeploymentPolicy {
        ReferenceDeploymentPolicy::new(vec![fixture.root.parent().expect("版本目录").to_path_buf()])
            .expect("允许根")
            .with_source_trust_requirement(SourceTrustRequirement::Unrestricted)
    }

    fn write_git_manifest(fixture: &Fixture, policy: &ReferenceDeploymentPolicy) -> SourceManifest {
        let validated =
            validate_source_root(policy, &fixture.root.to_string_lossy()).expect("源根");
        let scan = scan_source_tree(&validated, policy).expect("扫描");
        let entries = scan
            .entries()
            .iter()
            .filter(|entry| entry.kind != SourceEntryKind::Directory)
            .map(|entry| SourceManifestEntry {
                path: entry.path.clone(),
                git_mode: match entry.kind {
                    SourceEntryKind::Symlink => "120000".to_string(),
                    SourceEntryKind::File if entry.executable => "100755".to_string(),
                    SourceEntryKind::File | SourceEntryKind::Directory => "100644".to_string(),
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
                commit_sha: fixture.commit_sha.clone(),
                tree_sha: "9e56ed9f6d1b0a4ea2d2cf1c3f1c5f9b1d3a2c40".to_string(),
            },
            manifest_digest: tree_digest_from_entries(&entries),
            file_count: entries.len() as u64,
            total_bytes: scan.byte_count(),
            entries,
            note: None,
        };
        std::fs::write(
            fixture.version.join(SOURCE_MANIFEST_FILE_NAME),
            serde_json::to_vec_pretty(&manifest).expect("manifest JSON"),
        )
        .expect("写入 manifest");
        manifest
    }

    fn request(fixture: &Fixture) -> NewReferenceSourceRequest {
        NewReferenceSourceRequest {
            name: "Linux 驱动参考".to_string(),
            host_path: fixture.root.to_string_lossy().into_owned(),
            revision_label: "已发布的 Linux 固定版本".to_string(),
            approved_for_agent_access: true,
        }
    }

    #[test]
    fn git_manifest_source_reports_commit_provenance() {
        let fixture = make_fixture(&[("drivers/foo.c", b"int make_fixture(void) { return 1; }\n")]);
        let policy = fixture_policy(&fixture);
        let manifest = write_git_manifest(&fixture, &policy);

        let validated =
            validate_new_source(&policy, &request(&fixture), Utc::now()).expect("验证通过");
        assert_eq!(SourceProvenanceKind::GitTree, validated.provenance().kind);
        assert_eq!(
            Some(fixture.commit_sha.clone()),
            validated.provenance().commit_sha.clone()
        );
        assert_eq!(manifest.manifest_digest, validated.manifest_digest());
        assert_eq!(1, validated.entry_count());
        assert_eq!(1, validated.file_count());
        assert!(validated.warnings().is_empty());

        let record = ready_source_record(
            Uuid::new_v4(),
            &request(&fixture),
            &validated,
            validated.verified_at(),
            validated.verified_at(),
        );
        assert_eq!(SourceState::Ready, record.state);
        assert!(SourceAvailability::of(&record).is_usable());
    }

    #[test]
    fn plain_directory_without_manifest_is_a_directory_snapshot() {
        let fixture = make_fixture(&[("plain.c", b"int make_fixture(void) { return 1; }\n")]);
        let policy = fixture_policy(&fixture);

        let validated =
            validate_new_source(&policy, &request(&fixture), Utc::now()).expect("普通目录验证通过");
        assert_eq!(
            SourceProvenanceKind::DirectorySnapshot,
            validated.provenance().kind
        );
        assert!(validated.provenance().commit_sha.is_none());
        assert_eq!(1, validated.warnings().len());
        assert!(validated.manifest_digest().starts_with("sha256:"));
    }

    #[test]
    fn tampered_manifest_fails_validation() {
        let fixture = make_fixture(&[("plain.c", b"int make_fixture(void) { return 1; }\n")]);
        let policy = fixture_policy(&fixture);
        let manifest = write_git_manifest(&fixture, &policy);

        std::fs::write(fixture.root.join("plain.c"), b"int tampered(void);\n").expect("篡改内容");
        let error = validate_new_source(&policy, &request(&fixture), Utc::now())
            .expect_err("内容被改写必须失败");
        assert_eq!(ReferenceErrorCode::ManifestEntryMismatch, error.code());
        assert_eq!(1, manifest.file_count);
    }

    #[test]
    fn revalidation_detects_external_change() {
        // 有 manifest：内容被原地改写先被 manifest 与实测内容的逐项核对抓住。
        let fixture = make_fixture(&[("plain.c", b"int make_fixture(void) { return 1; }\n")]);
        let policy = fixture_policy(&fixture);
        write_git_manifest(&fixture, &policy);
        let validated =
            validate_new_source(&policy, &request(&fixture), Utc::now()).expect("验证通过");
        let record = ready_source_record(
            Uuid::new_v4(),
            &request(&fixture),
            &validated,
            validated.verified_at(),
            validated.verified_at(),
        );

        std::fs::write(fixture.root.join("plain.c"), b"int changed;\n").expect("外部修改");
        let error =
            revalidate_source(&policy, &record, Utc::now()).expect_err("原地修改必须被拒绝");
        assert_eq!(ReferenceErrorCode::ManifestEntryMismatch, error.code());

        // 没有 manifest 的目录快照：摘要变化表现为登记身份变化。
        let fixture = make_fixture(&[("plain.c", b"int make_fixture(void) { return 1; }\n")]);
        let policy = fixture_policy(&fixture);
        let validated =
            validate_new_source(&policy, &request(&fixture), Utc::now()).expect("验证通过");
        let record = ready_source_record(
            Uuid::new_v4(),
            &request(&fixture),
            &validated,
            validated.verified_at(),
            validated.verified_at(),
        );

        std::fs::write(fixture.root.join("plain.c"), b"int changed;\n").expect("外部修改");
        let error =
            revalidate_source(&policy, &record, Utc::now()).expect_err("原地修改必须被拒绝");
        assert_eq!(ReferenceErrorCode::SourceIdentityChanged, error.code());
    }

    #[test]
    fn names_and_labels_are_bounded() {
        let fixture = make_fixture(&[("plain.c", b"x\n")]);
        let policy = fixture_policy(&fixture);

        let mut blank = request(&fixture);
        blank.name = "   ".to_string();
        assert_eq!(
            ReferenceErrorCode::InvalidSourceName,
            validate_new_source(&policy, &blank, Utc::now())
                .expect_err("空名称")
                .code()
        );

        let mut long = request(&fixture);
        long.revision_label = "长".repeat(MAX_REVISION_LABEL_CHARS + 1);
        assert_eq!(
            ReferenceErrorCode::InvalidRevisionLabel,
            validate_new_source(&policy, &long, Utc::now())
                .expect_err("超长标签")
                .code()
        );
    }

    #[test]
    fn availability_follows_state_and_approval() {
        let fixture = make_fixture(&[("plain.c", b"x\n")]);
        let policy = fixture_policy(&fixture);
        let validated =
            validate_new_source(&policy, &request(&fixture), Utc::now()).expect("验证通过");
        let mut record = ready_source_record(
            Uuid::new_v4(),
            &request(&fixture),
            &validated,
            validated.verified_at(),
            validated.verified_at(),
        );

        record.approved_for_agent_access = false;
        assert_eq!(
            SourceAvailability::NotApproved,
            SourceAvailability::of(&record)
        );
        record.state = SourceState::Validating;
        assert_eq!(
            SourceAvailability::AwaitingValidation,
            SourceAvailability::of(&record)
        );
        record.state = SourceState::Missing;
        assert_eq!(
            SourceAvailability::Unavailable,
            SourceAvailability::of(&record)
        );
    }

    #[test]
    fn provenance_corroboration_reports_directory_mismatch() {
        let fixture = make_fixture(&[("plain.c", b"x\n")]);
        assert_eq!(
            ProvenanceCorroboration::Corroborated,
            provenance_corroboration(&fixture.root, &SourceProvenance::directory_snapshot())
        );
        assert_eq!(
            ProvenanceCorroboration::Corroborated,
            provenance_corroboration(
                &fixture.root,
                &SourceProvenance::git_tree(fixture.commit_sha.clone(), "a".repeat(40))
            )
        );
        assert_eq!(
            ProvenanceCorroboration::DirectoryNameMismatch {
                directory_name: "not-a-sha".to_string(),
                commit_sha: fixture.commit_sha.clone(),
            },
            provenance_corroboration(
                Path::new("/srv/refs/not-a-sha/repo"),
                &SourceProvenance::git_tree(fixture.commit_sha.clone(), "a".repeat(40))
            )
        );
    }
}
