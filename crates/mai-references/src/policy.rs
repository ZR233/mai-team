//! 部署侧策略与宿主路径/归属校验。
//!
//! 这里是参考源"能不能挂"的唯一判定点：
//! - 路径必须严格落在部署配置的允许根之内，按路径组件比较而不是字符串前缀。
//! - 登记根与全部祖先都不允许是符号链接；生产要求它们由可信 UID 持有且不可被
//!   组/其他用户写入，避免出现"目录被可信管理员之外的人替换"的授权歧义。
//!
//! 目录通过这里的检查之后，内容层面（文件类型、链接、摘要）的判定在
//! [`crate::tree`]，两者都不接受"跳过检查继续登记"。

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use mai_protocol::{
    ALIAS_PATTERN, DEFAULT_MAX_BINDINGS_PER_PROJECT, MAX_ALIAS_CHARS, MAX_DESCRIPTION_CHARS,
    MAX_REVISION_LABEL_CHARS, MAX_SOURCE_NAME_CHARS, REFERENCE_CONTAINER_ROOT, ReferenceErrorCode,
    ReferenceSourcePolicy, SourceProvenanceKind,
};

use crate::error::{ReferenceError, Result};
use crate::manifest::SOURCE_MANIFEST_FILE_NAME;

/// 宿主路径的字符上限，超过即拒绝，避免异常输入进入日志与 Docker 参数。
const MAX_HOST_PATH_CHARS: usize = 4096;

/// 默认的源码树扫描条目上限。
const DEFAULT_MAX_SCAN_ENTRIES: u64 = 4_000_000;

/// 严格递归只读能力对应的最低 Docker Engine 功能版本。
const MINIMUM_DOCKER_ENGINE_VERSION: &str = "25.0";

/// 严格递归只读能力对应的最低内核版本。
const MINIMUM_KERNEL_VERSION: &str = "5.12";

/// 源码归属与权限的信任要求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceTrustRequirement {
    /// 生产要求：源根与全部祖先由可信 UID 持有，且不允许组/其他用户写入。
    TrustedOwnerAndPermissions,
    /// 仅用于离线夹具与单元测试：跳过归属与权限检查，路径、类型与链接检查仍然执行。
    Unrestricted,
}

/// 树内单个节点的归属/权限元数据。
///
/// 真实扫描用 [`TreeNodeTrust::from_metadata`] 从 `symlink_metadata` 得到；测试可以直接
/// 构造（注入 uid/mode），因此不需要 chmod/chown，也不依赖进程环境变量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeNodeTrust {
    /// 节点持有者 UID。
    pub uid: u32,
    /// 节点权限位；符号链接恒为 `0o777`，因此不参与判断。
    pub mode: u32,
    /// 该节点是否为符号链接。
    pub symlink: bool,
}

impl TreeNodeTrust {
    /// 从真实文件系统元数据构造；符号链接按链接自身判断。
    pub fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            uid: metadata.uid(),
            mode: metadata.mode(),
            symlink: metadata.file_type().is_symlink(),
        }
    }
}

/// 部署侧参考源策略；只从部署配置构造，项目 UI 不能修改。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceDeploymentPolicy {
    allowed_roots: Vec<PathBuf>,
    trusted_owner_uids: Vec<u32>,
    source_trust_requirement: SourceTrustRequirement,
    max_bindings_per_project: u16,
    max_description_chars: usize,
    max_scan_entries: u64,
    require_strict_recursive_read_only: bool,
}

impl ReferenceDeploymentPolicy {
    /// 用允许根构造策略；允许根必须是规范绝对路径，且不允许是文件系统根。
    pub fn new(allowed_roots: Vec<PathBuf>) -> Result<Self> {
        if allowed_roots.is_empty() {
            return Err(ReferenceError::new(
                ReferenceErrorCode::SourcePathInvalid,
                "参考源功能缺少部署配置的允许根",
            ));
        }
        let mut normalized = Vec::with_capacity(allowed_roots.len());
        for allowed_root in allowed_roots {
            let text = allowed_root.to_str().ok_or_else(|| {
                ReferenceError::new(
                    ReferenceErrorCode::SourcePathInvalid,
                    "允许根必须是 UTF-8 绝对路径",
                )
            })?;
            normalized.push(validate_host_path_syntax(text).map_err(|error| {
                ReferenceError::new(
                    ReferenceErrorCode::SourcePathInvalid,
                    format!("部署配置的允许根不合法：{text}（{}）", error.message()),
                )
            })?);
        }
        Ok(Self {
            allowed_roots: normalized,
            trusted_owner_uids: vec![0],
            source_trust_requirement: SourceTrustRequirement::TrustedOwnerAndPermissions,
            max_bindings_per_project: DEFAULT_MAX_BINDINGS_PER_PROJECT,
            max_description_chars: MAX_DESCRIPTION_CHARS,
            max_scan_entries: DEFAULT_MAX_SCAN_ENTRIES,
            require_strict_recursive_read_only: true,
        })
    }

    /// 设置可信持有者 UID 集合。
    pub fn with_trusted_owner_uids(mut self, uids: Vec<u32>) -> Self {
        self.trusted_owner_uids = uids;
        self
    }

    /// 设置源码归属与权限的信任要求。
    pub fn with_source_trust_requirement(mut self, requirement: SourceTrustRequirement) -> Self {
        self.source_trust_requirement = requirement;
        self
    }

    /// 设置单项目最多绑定条数。
    pub fn with_max_bindings_per_project(mut self, max: u16) -> Self {
        self.max_bindings_per_project = max;
        self
    }

    /// 设置绑定说明的字符上限。
    pub fn with_max_description_chars(mut self, max: usize) -> Self {
        self.max_description_chars = max;
        self
    }

    /// 设置扫描条目上限。
    pub fn with_max_scan_entries(mut self, max: u64) -> Self {
        self.max_scan_entries = max;
        self
    }

    /// 声明部署环境尚未通过严格递归只读能力验证。
    pub fn without_strict_recursive_read_only(mut self) -> Self {
        self.require_strict_recursive_read_only = false;
        self
    }

    /// 允许根列表。
    pub fn allowed_roots(&self) -> &[PathBuf] {
        &self.allowed_roots
    }

    /// 可信持有者 UID 列表。
    pub fn trusted_owner_uids(&self) -> &[u32] {
        &self.trusted_owner_uids
    }

    /// 源码归属与权限的信任要求。
    pub fn source_trust_requirement(&self) -> SourceTrustRequirement {
        self.source_trust_requirement
    }

    /// 单项目最多绑定条数。
    pub fn max_bindings_per_project(&self) -> u16 {
        self.max_bindings_per_project
    }

    /// 绑定说明的字符上限。
    pub fn max_description_chars(&self) -> usize {
        self.max_description_chars
    }

    /// 扫描条目上限。
    pub fn max_scan_entries(&self) -> u64 {
        self.max_scan_entries
    }

    /// 部署是否已声明必须使用严格递归只读挂载。
    pub fn require_strict_recursive_read_only(&self) -> bool {
        self.require_strict_recursive_read_only
    }

    /// 生成管理面可见的能力与限制描述；不含任何凭据。
    pub fn public_policy(&self) -> ReferenceSourcePolicy {
        ReferenceSourcePolicy {
            allowed_roots: self
                .allowed_roots
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            trusted_owner_uids: self.trusted_owner_uids.clone(),
            reference_container_root: REFERENCE_CONTAINER_ROOT.to_string(),
            alias_pattern: ALIAS_PATTERN.to_string(),
            max_alias_chars: MAX_ALIAS_CHARS as u16,
            max_bindings_per_project: self.max_bindings_per_project,
            max_description_chars: self.max_description_chars as u16,
            source_name_max_chars: MAX_SOURCE_NAME_CHARS as u16,
            revision_label_max_chars: MAX_REVISION_LABEL_CHARS as u16,
            manifest_file_name: SOURCE_MANIFEST_FILE_NAME.to_string(),
            supported_provenance_kinds: vec![
                SourceProvenanceKind::GitTree,
                SourceProvenanceKind::DirectorySnapshot,
            ],
            require_strict_recursive_read_only: self.require_strict_recursive_read_only,
            minimum_docker_engine_version: MINIMUM_DOCKER_ENGINE_VERSION.to_string(),
            minimum_kernel_version: MINIMUM_KERNEL_VERSION.to_string(),
        }
    }
}

/// 通过语法与文件系统校验的宿主源码根；只能由本模块构造。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSourceRoot {
    path: PathBuf,
    allowed_root: PathBuf,
    device: u64,
    inode: u64,
    owner_uid: u32,
}

impl ValidatedSourceRoot {
    /// 规范宿主路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 该路径命中的部署允许根。
    pub fn allowed_root(&self) -> &Path {
        &self.allowed_root
    }

    /// 目录所在设备号；用于拒绝内部子挂载。
    pub fn device(&self) -> u64 {
        self.device
    }

    /// 目录 inode；与设备号一起构成登记身份的一部分。
    pub fn inode(&self) -> u64 {
        self.inode
    }

    /// 目录持有者 UID。
    pub fn owner_uid(&self) -> u32 {
        self.owner_uid
    }
}

/// 校验宿主路径语法并返回规范化绝对路径；不访问文件系统。
pub fn validate_host_path_syntax(host_path: &str) -> Result<PathBuf> {
    if host_path.is_empty() || host_path.trim() != host_path {
        return Err(invalid_path("宿主路径不能为空，且不能有首尾空白"));
    }
    if host_path.chars().count() > MAX_HOST_PATH_CHARS {
        return Err(invalid_path("宿主路径过长"));
    }
    if !host_path.starts_with('/') {
        return Err(invalid_path("宿主路径必须是绝对路径"));
    }
    if host_path.contains(',') {
        return Err(invalid_path("宿主路径不能包含逗号（Docker 挂载分隔符）"));
    }
    if host_path.contains('~') {
        return Err(invalid_path("宿主路径不接受 ~ 展开"));
    }
    if host_path
        .chars()
        .any(|character| (character as u32) < 0x20 || (character as u32) == 0x7f)
    {
        return Err(invalid_path("宿主路径不能包含控制字符"));
    }

    let mut normalized = PathBuf::from("/");
    let mut depth = 0usize;
    for component in Path::new(host_path).components() {
        match component {
            Component::RootDir => {}
            Component::CurDir => {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::UnsafePathComponent,
                    "宿主路径不能包含 . 组件",
                ));
            }
            Component::ParentDir => {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::UnsafePathComponent,
                    "宿主路径不能包含 .. 组件",
                ));
            }
            Component::Prefix(_) => {
                return Err(invalid_path("不支持 Windows 前缀路径"));
            }
            Component::Normal(name) => {
                let name = name
                    .to_str()
                    .ok_or_else(|| invalid_path("宿主路径必须可表示为 UTF-8"))?;
                if name.is_empty() {
                    return Err(invalid_path("宿主路径包含空组件"));
                }
                if name == ".git" {
                    return Err(ReferenceError::new(
                        ReferenceErrorCode::UnsafePathComponent,
                        "宿主路径不能包含 .git 组件",
                    ));
                }
                normalized.push(name);
                depth += 1;
            }
        }
    }
    if depth == 0 {
        return Err(invalid_path("不能把文件系统根作为参考源"));
    }
    Ok(normalized)
}

/// 校验宿主源码根：允许根边界、祖先无符号链接、归属与权限、目录类型与身份稳定。
pub fn validate_source_root(
    policy: &ReferenceDeploymentPolicy,
    host_path: &str,
) -> Result<ValidatedSourceRoot> {
    let path = validate_host_path_syntax(host_path)?;
    let allowed_root = match_allowed_root(&path, policy.allowed_roots())?;

    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ReferenceError::new(
                ReferenceErrorCode::SourceNotFound,
                format!("参考源目录不存在：{}", path.display()),
            )
        } else {
            ReferenceError::from_io(
                ReferenceErrorCode::SourceUnreadable,
                format!("无法读取参考源目录：{}", path.display()),
                error,
            )
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(unsafe_symlink(format!(
            "参考源根不能是符号链接：{}",
            path.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(ReferenceError::new(
            ReferenceErrorCode::SourceNotDirectory,
            format!("参考源路径不是目录：{}", path.display()),
        ));
    }

    validate_ancestor_chain(policy, &path)?;

    let canonical = fs::canonicalize(&path).map_err(|error| {
        ReferenceError::from_io(
            ReferenceErrorCode::SourceUnreadable,
            format!("无法规范化参考源路径：{}", path.display()),
            error,
        )
    })?;
    if canonical != path {
        return Err(unsafe_symlink(format!(
            "参考源路径包含符号链接别名：{} -> {}",
            path.display(),
            canonical.display()
        )));
    }

    let confirmed = fs::symlink_metadata(&path).map_err(|error| {
        ReferenceError::from_io(
            ReferenceErrorCode::SourceUnreadable,
            format!("无法重新读取参考源目录：{}", path.display()),
            error,
        )
    })?;
    if confirmed.dev() != metadata.dev() || confirmed.ino() != metadata.ino() {
        return Err(ReferenceError::new(
            ReferenceErrorCode::SourceIdentityChanged,
            format!("参考源目录在验证过程中被替换：{}", path.display()),
        ));
    }

    if !confirmed.is_dir() {
        return Err(ReferenceError::new(
            ReferenceErrorCode::SourceNotDirectory,
            format!("参考源路径不是目录：{}", path.display()),
        ));
    }

    Ok(ValidatedSourceRoot {
        path,
        allowed_root,
        device: confirmed.dev(),
        inode: confirmed.ino(),
        owner_uid: confirmed.uid(),
    })
}

fn match_allowed_root(path: &Path, allowed_roots: &[PathBuf]) -> Result<PathBuf> {
    let depth = path.components().count();
    let mut matched: Option<&PathBuf> = None;
    for allowed_root in allowed_roots {
        let allowed_depth = allowed_root.components().count();
        if depth > allowed_depth && path.starts_with(allowed_root) {
            let better = matched
                .map(|current| allowed_depth > current.components().count())
                .unwrap_or(true);
            if better {
                matched = Some(allowed_root);
            }
        }
    }
    matched.cloned().ok_or_else(|| {
        ReferenceError::new(
            ReferenceErrorCode::SourceOutsideAllowedRoot,
            format!("参考源路径不在部署配置的允许根内：{}", path.display()),
        )
    })
}

fn validate_ancestor_chain(policy: &ReferenceDeploymentPolicy, path: &Path) -> Result<()> {
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut current = PathBuf::from("/");
    for component in path.components().skip(1) {
        current.push(component.as_os_str());
        chain.push(current.clone());
    }

    let trust_required =
        policy.source_trust_requirement() == SourceTrustRequirement::TrustedOwnerAndPermissions;
    for ancestor in chain {
        let metadata = fs::symlink_metadata(&ancestor).map_err(|error| {
            ReferenceError::from_io(
                ReferenceErrorCode::SourceUnreadable,
                format!("无法读取路径组件：{}", ancestor.display()),
                error,
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(unsafe_symlink(format!(
                "参考源路径组件不能是符号链接：{}",
                ancestor.display()
            )));
        }
        if !metadata.is_dir() {
            return Err(ReferenceError::new(
                ReferenceErrorCode::SourceNotDirectory,
                format!("参考源路径组件不是目录：{}", ancestor.display()),
            ));
        }
        if trust_required {
            if !policy.trusted_owner_uids().contains(&metadata.uid()) {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::UntrustedSourceOwner,
                    format!(
                        "参考源路径组件必须由可信 UID 持有：{}（当前 uid {}）",
                        ancestor.display(),
                        metadata.uid()
                    ),
                ));
            }
            if metadata.mode() & 0o022 != 0 {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::UntrustedSourceOwner,
                    format!(
                        "参考源路径组件不允许组或其他用户写入：{}",
                        ancestor.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// 校验源码树内单个节点的归属与权限。
///
/// 只检查源根与祖先不够：树内任何一个文件或目录如果由不可信 UID 持有、或其权限允许组/
/// 其他用户写入，就等于把"内容不可被非可信用户替换"的前提破掉，这样的树不能转 ready。
///
/// 符号链接只校验持有者：链接自身的权限位恒为 `0o777` 且无法用于改写目标，写保护来自
/// 可信且不可写的父目录，以及"目标必须仍位于同一参考源内"的链接解析规则。
///
/// [`SourceTrustRequirement::Unrestricted`] 跳过全部检查，只用于离线夹具与单元测试。
pub fn validate_tree_node_trust(
    policy: &ReferenceDeploymentPolicy,
    relative_path: &str,
    node: &TreeNodeTrust,
) -> Result<()> {
    if policy.source_trust_requirement() == SourceTrustRequirement::Unrestricted {
        return Ok(());
    }
    if !policy.trusted_owner_uids().contains(&node.uid) {
        return Err(ReferenceError::new(
            ReferenceErrorCode::UntrustedSourceOwner,
            format!(
                "源码树内条目必须由可信 UID 持有：{relative_path}（当前 uid {}）",
                node.uid
            ),
        ));
    }
    if node.symlink {
        return Ok(());
    }
    if node.mode & 0o022 != 0 {
        return Err(ReferenceError::new(
            ReferenceErrorCode::UntrustedSourceOwner,
            format!(
                "源码树内条目不允许组或其他用户写入：{relative_path}（mode {:04o}）",
                node.mode
            ),
        ));
    }
    Ok(())
}

fn invalid_path(message: &str) -> ReferenceError {
    ReferenceError::new(ReferenceErrorCode::SourcePathInvalid, message)
}

pub(crate) fn unsafe_symlink(message: impl Into<String>) -> ReferenceError {
    ReferenceError::new(ReferenceErrorCode::UnsafeSymlink, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::ReferenceErrorCode;
    use pretty_assertions::assert_eq;
    use std::os::unix::fs::PermissionsExt;

    fn test_policy(root: &Path) -> ReferenceDeploymentPolicy {
        ReferenceDeploymentPolicy::new(vec![root.to_path_buf()])
            .expect("允许根")
            .with_source_trust_requirement(SourceTrustRequirement::Unrestricted)
    }

    #[test]
    fn path_syntax_rejects_dangerous_input() {
        assert_eq!(
            ReferenceErrorCode::SourcePathInvalid,
            validate_host_path_syntax("").expect_err("空路径").code()
        );
        assert_eq!(
            ReferenceErrorCode::SourcePathInvalid,
            validate_host_path_syntax("relative/path")
                .expect_err("相对路径")
                .code()
        );
        assert_eq!(
            ReferenceErrorCode::SourcePathInvalid,
            validate_host_path_syntax("/srv/ref,ro")
                .expect_err("逗号")
                .code()
        );
        assert_eq!(
            ReferenceErrorCode::SourcePathInvalid,
            validate_host_path_syntax("/srv/~ref")
                .expect_err("~")
                .code()
        );
        assert_eq!(
            ReferenceErrorCode::UnsafePathComponent,
            validate_host_path_syntax("/srv/ref/../evil")
                .expect_err("..")
                .code()
        );
        assert_eq!(
            ReferenceErrorCode::UnsafePathComponent,
            validate_host_path_syntax("/srv/ref/.git/config")
                .expect_err(".git")
                .code()
        );
        assert_eq!(
            PathBuf::from("/srv/mai-references/linux/repo"),
            validate_host_path_syntax("/srv/mai-references/linux/repo").expect("合法路径")
        );
        assert_eq!(
            PathBuf::from("/srv/mai-references/linux/repo"),
            validate_host_path_syntax("/srv/mai-references/linux/repo/").expect("尾部分隔符")
        );
    }

    #[test]
    fn allowed_root_matching_uses_component_boundaries() {
        let temp = tempfile::tempdir().expect("临时目录");
        let allowed = temp.path().join("ref");
        let sibling = temp.path().join("ref-evil");
        std::fs::create_dir_all(&allowed).expect("允许根");
        std::fs::create_dir_all(&sibling).expect("相似目录");
        let policy = test_policy(&allowed);

        assert_eq!(
            ReferenceErrorCode::SourceOutsideAllowedRoot,
            validate_source_root(&policy, &sibling.to_string_lossy())
                .expect_err("仅前缀相同不算在内")
                .code()
        );
        assert_eq!(
            ReferenceErrorCode::SourceOutsideAllowedRoot,
            validate_source_root(&policy, &allowed.to_string_lossy())
                .expect_err("允许根本身不能作为源")
                .code()
        );
        let nested = allowed.join("linux");
        std::fs::create_dir_all(&nested).expect("嵌套源");
        let validated = validate_source_root(&policy, &nested.to_string_lossy()).expect("允许根内");
        assert_eq!(nested, validated.path());
        assert_eq!(allowed, validated.allowed_root());
    }

    #[test]
    fn source_root_rejects_file_and_missing_path() {
        let temp = tempfile::tempdir().expect("临时目录");
        let policy = test_policy(temp.path());
        let file = temp.path().join("file.txt");
        std::fs::write(&file, b"x").expect("文件");

        assert_eq!(
            ReferenceErrorCode::SourceNotDirectory,
            validate_source_root(&policy, &file.to_string_lossy())
                .expect_err("文件不是目录")
                .code()
        );
        assert_eq!(
            ReferenceErrorCode::SourceNotFound,
            validate_source_root(&policy, &temp.path().join("missing").to_string_lossy())
                .expect_err("不存在")
                .code()
        );
    }

    #[test]
    fn public_policy_exposes_capabilities() {
        let temp = tempfile::tempdir().expect("临时目录");
        let policy = test_policy(temp.path());
        let public = policy.public_policy();
        assert_eq!(
            vec![temp.path().to_string_lossy().into_owned()],
            public.allowed_roots
        );
        assert_eq!(
            DEFAULT_MAX_BINDINGS_PER_PROJECT,
            public.max_bindings_per_project
        );
        assert_eq!(REFERENCE_CONTAINER_ROOT, public.reference_container_root);
        assert!(public.require_strict_recursive_read_only);
        assert_eq!(2, public.supported_provenance_kinds.len());
    }

    #[test]
    fn tree_node_trust_accepts_only_trusted_non_writable_nodes() {
        let policy = ReferenceDeploymentPolicy::new(vec![PathBuf::from("/srv/mai-references")])
            .expect("允许根");

        let allowed = [
            TreeNodeTrust {
                uid: 0,
                mode: 0o444,
                symlink: false,
            },
            TreeNodeTrust {
                uid: 0,
                mode: 0o555,
                symlink: false,
            },
            TreeNodeTrust {
                uid: 0,
                mode: 0o644,
                symlink: false,
            },
            TreeNodeTrust {
                uid: 0,
                mode: 0o755,
                symlink: false,
            },
            TreeNodeTrust {
                uid: 0,
                mode: 0o1755,
                symlink: false,
            },
            // 符号链接权限位恒为 0o777，不能用它一概拒绝；安全由父目录与同源解析保证。
            TreeNodeTrust {
                uid: 0,
                mode: 0o777,
                symlink: true,
            },
        ];
        for node in allowed {
            assert!(
                validate_tree_node_trust(&policy, "drivers/foo.c", &node).is_ok(),
                "应放行：{node:?}"
            );
        }

        let rejected = [
            (
                TreeNodeTrust {
                    uid: 1000,
                    mode: 0o444,
                    symlink: false,
                },
                "非可信 UID 的文件",
            ),
            (
                TreeNodeTrust {
                    uid: 0,
                    mode: 0o664,
                    symlink: false,
                },
                "组可写文件",
            ),
            (
                TreeNodeTrust {
                    uid: 0,
                    mode: 0o646,
                    symlink: false,
                },
                "其他用户可写文件",
            ),
            (
                TreeNodeTrust {
                    uid: 0,
                    mode: 0o775,
                    symlink: false,
                },
                "组可写目录",
            ),
            (
                TreeNodeTrust {
                    uid: 0,
                    mode: 0o1777,
                    symlink: false,
                },
                "类似 /tmp 的目录",
            ),
            (
                TreeNodeTrust {
                    uid: 1000,
                    mode: 0o777,
                    symlink: true,
                },
                "非可信 UID 的符号链接",
            ),
        ];
        for (node, reason) in rejected {
            let error = validate_tree_node_trust(&policy, "drivers/foo.c", &node)
                .expect_err(&format!("必须拒绝：{reason}"));
            assert_eq!(ReferenceErrorCode::UntrustedSourceOwner, error.code());
        }
    }

    #[test]
    fn unrestricted_policy_skips_tree_node_trust() {
        let policy = ReferenceDeploymentPolicy::new(vec![PathBuf::from("/srv/mai-references")])
            .expect("允许根")
            .with_source_trust_requirement(SourceTrustRequirement::Unrestricted);
        let node = TreeNodeTrust {
            uid: 1000,
            mode: 0o666,
            symlink: false,
        };
        assert!(validate_tree_node_trust(&policy, "drivers/foo.c", &node).is_ok());
    }

    #[test]
    fn production_trust_rejects_untrusted_or_writable_ancestors() {
        // 夹具显式造一个"其他用户可写"的祖先：整条链都必须可信且不可写，
        // 因此临时目录夹具必须改用 Unrestricted，而不是放宽生产判据。
        let temp = tempfile::tempdir().expect("临时目录");
        let allowed = temp.path().join("allowed");
        let root = allowed.join("repo");
        std::fs::create_dir_all(&root).expect("源码根");
        std::fs::set_permissions(&allowed, std::fs::Permissions::from_mode(0o777))
            .expect("设置权限");
        let policy = ReferenceDeploymentPolicy::new(vec![allowed]).expect("允许根");
        assert_eq!(
            ReferenceErrorCode::UntrustedSourceOwner,
            validate_source_root(&policy, &root.to_string_lossy())
                .expect_err("可被其他用户写入的祖先必须被拒绝")
                .code()
        );
    }
}
