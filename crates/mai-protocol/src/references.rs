//! 项目参考源码（管理员发布的固定版本只读源码目录）的公共协议类型。
//!
//! 这些 DTO 同时是 HTTP 契约、存储 JSON 与 Web UI 的唯一类型来源。所有结构都
//! 严格拒绝未知字段：参考源配置是安全边界，静默忽略未知字段既可能丢掉未授权
//! 字段，也可能让客户端以为某个字段已被接受。
//!
//! 边界约定：
//! - `host_path` 只出现在管理面 DTO 中，绝不进入模型可见文本。
//! - `revision`/`applied_revision` 是 JS 安全整数，禁止溢出后静默改版本。
//! - 请求中的 `bindings` 必须显式给出：缺失或 `null` 都不是"清空"。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AgentId, ProjectId};

/// 容器内参考源码的保留命名空间；项目与绑定都不能自定义挂载目标。
pub const REFERENCE_CONTAINER_ROOT: &str = "/references";

/// alias 匹配规则：小写字母开头，其余仅小写字母/数字/下划线/连字符，总长不超过 32。
pub const ALIAS_PATTERN: &str = "^[a-z][a-z0-9_-]{0,31}$";

/// alias 允许的最大字符数。
pub const MAX_ALIAS_CHARS: usize = 32;

/// 单项目默认最多绑定条数（包含禁用条目）。
pub const DEFAULT_MAX_BINDINGS_PER_PROJECT: u16 = 16;

/// 绑定说明允许的最大 Unicode 字符数。
pub const MAX_DESCRIPTION_CHARS: usize = 512;

/// 参考源名称允许的最大 Unicode 字符数。
pub const MAX_SOURCE_NAME_CHARS: usize = 128;

/// 版本标签允许的最大 Unicode 字符数。
pub const MAX_REVISION_LABEL_CHARS: usize = 256;

/// JS 安全整数上限（2^53 - 1）；配置修订号必须落在该范围内。
pub const MAX_JS_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// 项目引用状态查询默认返回的 Agent 条数。
pub const DEFAULT_REFERENCE_STATUS_LIMIT: u64 = 100;

/// 项目引用状态查询允许的最大 Agent 条数。
pub const MAX_REFERENCE_STATUS_LIMIT: u64 = 500;

/// 返回 `DEFAULT_MAX_BINDINGS_PER_PROJECT`，供部署配置缺省值使用。
pub fn default_max_bindings_per_project() -> u16 {
    DEFAULT_MAX_BINDINGS_PER_PROJECT
}

/// 参考源登记的验证生命周期。
///
/// `Validating` 只在受控验证 worker 完成前存在；`Missing` 表示宿主目录当前不可用，
/// 已经绑定的项目保留条目但不能再启用。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SourceState {
    Validating,
    Ready,
    Invalid,
    Disabled,
    Missing,
}

impl SourceState {
    /// 只有 `Ready` 的源可以被项目启用。
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// 验证已经结束（成功或失败）的终态。
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Validating => false,
            Self::Ready | Self::Invalid | Self::Disabled | Self::Missing => true,
        }
    }
}

/// 发布来源的种类。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SourceProvenanceKind {
    /// 由可信发布器从本地 Git commit 导出的快照，目录名与 manifest 共同证明来源。
    GitTree,
    /// 管理员维护的普通目录，只有本目录自己的内容摘要，不声称对应任何 commit。
    DirectorySnapshot,
}

/// 参考源的发布来源。
///
/// `GitTree` 必须同时携带 `commit_sha` 与 `tree_sha`；`DirectorySnapshot` 不携带
/// 任何 Git 标识。JSON 层用扁平结构加严格未知字段检查，避免内部 tag 枚举忽略多余字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceProvenance {
    pub kind: SourceProvenanceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_sha: Option<String>,
}

impl SourceProvenance {
    /// 构造 Git 快照来源。
    pub fn git_tree(commit_sha: impl Into<String>, tree_sha: impl Into<String>) -> Self {
        Self {
            kind: SourceProvenanceKind::GitTree,
            commit_sha: Some(commit_sha.into()),
            tree_sha: Some(tree_sha.into()),
        }
    }

    /// 构造普通目录快照来源。
    pub fn directory_snapshot() -> Self {
        Self {
            kind: SourceProvenanceKind::DirectorySnapshot,
            commit_sha: None,
            tree_sha: None,
        }
    }
}

/// 一次验证失败的结构化记录；人类消息与稳定机器码分离。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceValidationFailure {
    pub code: ReferenceErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub observed_at: DateTime<Utc>,
}

/// 管理面看到的参考源登记项。
///
/// `host_path` 只用于管理 UI 与部署校验，不进模型上下文。`source_device`/`source_inode`
/// 与 `manifest_digest` 一起构成登记身份，登记后不可通过 PATCH 修改。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceSource {
    pub id: Uuid,
    pub name: String,
    pub revision_label: String,
    pub host_path: String,
    pub provenance: SourceProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_digest: Option<String>,
    pub state: SourceState,
    pub approved_for_agent_access: bool,
    pub file_count: u64,
    pub byte_count: u64,
    pub source_device: u64,
    pub source_inode: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_error: Option<ReferenceValidationFailure>,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 登记新参考源的请求。
///
/// 请求只给出宿主路径；版本来源与摘要由服务端验证器读取 manifest 后写入，
/// 不信任客户端声明的 commit。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewReferenceSourceRequest {
    pub name: String,
    pub host_path: String,
    pub revision_label: String,
    pub approved_for_agent_access: bool,
}

/// 修改参考源元数据的请求；路径、来源与摘要都不在可改字段内。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PatchReferenceSourceRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_for_agent_access: Option<bool>,
    /// 置为 `true` 表示全局禁用；服务端仍要求没有任何项目绑定。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
}

impl PatchReferenceSourceRequest {
    /// 请求没有携带任何可改字段。
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.revision_label.is_none()
            && self.approved_for_agent_access.is_none()
            && self.disabled.is_none()
    }
}

/// 项目对某个固定版本参考源的绑定条目。
///
/// `id` 是稳定条目 ID（同时是 UI key），`alias` 决定容器内目标
/// `/references/<alias>`；绑定不携带任何读写开关。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectReferenceBinding {
    pub id: Uuid,
    pub source_id: Uuid,
    pub alias: String,
    pub description: String,
    pub enabled: bool,
}

/// 项目的规范化参考源配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectReferenceConfiguration {
    pub revision: u64,
    pub bindings: Vec<ProjectReferenceBinding>,
}

impl ProjectReferenceConfiguration {
    /// 项目的空配置：迁移旧项目与新建项目都使用同一形状。
    pub fn empty() -> Self {
        Self {
            revision: 0,
            bindings: Vec::new(),
        }
    }
}

/// 整体替换项目参考源配置的请求（CAS）。
///
/// `bindings` 必须显式给出：缺失或 `null` 都是请求错误，`[]` 才表示清空。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceProjectReferencesRequest {
    pub expected_revision: u64,
    pub bindings: Vec<ProjectReferenceBinding>,
}

/// 应用策略：默认等当前 turn 结束，或由管理员确认后停止再应用。
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ReferenceApplyPolicy {
    #[default]
    WhenIdle,
    StopAndApply,
}

/// 申请立即重试/停止并应用某个已经 durable 的配置版本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyProjectReferencesRequest {
    pub expected_revision: u64,
    #[serde(default)]
    pub policy: ReferenceApplyPolicy,
}

/// 单个 Agent 的换代阶段。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ReferenceApplyPhase {
    WaitingIdle,
    Draining,
    Snapshotting,
    Creating,
    Verifying,
    Rebinding,
    Publishing,
    CleanupPending,
    Succeeded,
    Failed,
    Superseded,
}

impl ReferenceApplyPhase {
    /// 换代是否已经结束（成功、失败或被新目标取代）。
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::WaitingIdle
            | Self::Draining
            | Self::Snapshotting
            | Self::Creating
            | Self::Verifying
            | Self::Rebinding
            | Self::Publishing
            | Self::CleanupPending => false,
            Self::Succeeded | Self::Failed | Self::Superseded => true,
        }
    }
}

/// 参考目录的挂载访问方式；目前只有只读，保留位用于显式拒绝可写模式。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ReferenceAccess {
    ReadOnly,
}

/// 一次应用操作的可恢复收据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceApplyOperation {
    pub id: Uuid,
    pub agent_id: AgentId,
    pub project_id: ProjectId,
    pub phase: ReferenceApplyPhase,
    pub policy: ReferenceApplyPolicy,
    pub target_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_container_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_container_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_image_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ReferenceValidationFailure>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 已经应用到某个具体 generation 的单个参考源。
///
/// `host_path` 供 Docker 生成 bind 参数使用，只存在于服务端快照；模型可见投影
/// （`ModelVisibleReference`）不含该字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedReference {
    pub binding_id: Uuid,
    pub source_id: Uuid,
    pub name: String,
    pub revision_label: String,
    /// 版本标识：Git 快照为完整 commit SHA，普通目录为目录摘要。
    pub version: String,
    pub alias: String,
    pub container_path: String,
    pub host_path: String,
    pub tree_digest: String,
    pub provenance: SourceProvenance,
    pub source_device: u64,
    pub source_inode: u64,
    pub description: String,
    pub access: ReferenceAccess,
}

/// 某个 generation 实际生效的参考源集合；只包含 enabled 条目。
///
/// `fingerprint` 只覆盖挂载身份（源、摘要、路径、dev/ino、目标、访问方式），
/// 不含名称、说明与排序，因此"仅改说明"不会触发换代。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedReferenceSnapshot {
    pub project_id: ProjectId,
    pub configuration_revision: u64,
    pub fingerprint: String,
    pub references: Vec<AppliedReference>,
    pub observed_at: DateTime<Utc>,
}

impl AppliedReferenceSnapshot {
    /// 已应用的有效参考源条数。
    pub fn len(&self) -> usize {
        self.references.len()
    }

    /// 该 generation 没有生效任何参考源（对应清空配置）。
    pub fn is_empty(&self) -> bool {
        self.references.is_empty()
    }
}

/// 允许进入模型上下文的参考源投影；不含任何宿主路径或凭据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelVisibleReference {
    pub source_id: Uuid,
    pub name: String,
    pub version: String,
    pub container_path: String,
    pub description: String,
    pub access: ReferenceAccess,
}

/// 单个 Agent 的应用状态。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ReferenceApplicationState {
    Applied,
    Pending,
    Applying,
    WaitingIdle,
    Failed,
    Deferred,
    /// 历史对象已经没有执行环境；恢复时才应用当前目标。
    NoExecutionEnvironment,
}

/// 单个 Agent 的参考源应用状态投影。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReferenceApplication {
    pub agent_id: AgentId,
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_image_id: Option<String>,
    pub applied_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_fingerprint: Option<String>,
    pub state: ReferenceApplicationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<ReferenceApplyOperation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ReferenceValidationFailure>,
    pub observed_at: DateTime<Utc>,
}

/// 项目级应用计数；统计完整集合，不受分页限制。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceApplicationCounts {
    pub applied: u64,
    pub waiting: u64,
    pub failed: u64,
    pub deferred: u64,
    pub no_environment: u64,
}

/// 项目级应用聚合状态。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProjectReferenceApplicationState {
    Pending,
    Applied,
    Failed,
    Deferred,
}

/// Agent 列表的分页信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferencePageInfo {
    pub offset: u64,
    pub limit: u64,
    /// 完整集合条数，不等于本页条数。
    pub total: u64,
}

/// `GET /projects/{id}/references/status` 查询参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReferenceStatusQuery {
    pub offset: u64,
    pub limit: u64,
}

impl Default for ReferenceStatusQuery {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: DEFAULT_REFERENCE_STATUS_LIMIT,
        }
    }
}

impl ReferenceStatusQuery {
    /// 把请求参数收敛到允许范围；`limit` 超过上限时按上限截断。
    pub fn normalized(&self) -> Self {
        Self {
            offset: self.offset,
            limit: self.limit.clamp(1, MAX_REFERENCE_STATUS_LIMIT),
        }
    }
}

/// 项目参考源状态响应：durable 配置 + 实际应用状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectReferenceStatus {
    pub project_id: ProjectId,
    pub configuration: ProjectReferenceConfiguration,
    pub target_revision: u64,
    pub state: ProjectReferenceApplicationState,
    pub counts: ReferenceApplicationCounts,
    pub agents: Vec<AgentReferenceApplication>,
    pub page: ReferencePageInfo,
    pub observed_at: DateTime<Utc>,
}

/// 部署侧参考源能力与限制；管理 UI 只能读取，不能扩大允许根。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceSourcePolicy {
    pub allowed_roots: Vec<String>,
    pub trusted_owner_uids: Vec<u32>,
    pub reference_container_root: String,
    pub alias_pattern: String,
    pub max_alias_chars: u16,
    pub max_bindings_per_project: u16,
    pub max_description_chars: u16,
    pub source_name_max_chars: u16,
    pub revision_label_max_chars: u16,
    pub manifest_file_name: String,
    pub supported_provenance_kinds: Vec<SourceProvenanceKind>,
    /// 严格要求递归只读；为 `false` 时代表该能力尚未通过环境验证。
    pub require_strict_recursive_read_only: bool,
    pub minimum_docker_engine_version: String,
    pub minimum_kernel_version: String,
}

/// 参考源领域的稳定错误机器码；人类消息单独给出。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ReferenceErrorCode {
    // 请求与配置校验（422）
    InvalidRequest,
    InvalidAlias,
    DuplicateAlias,
    DuplicateBindingId,
    DuplicateSourceBinding,
    TooManyBindings,
    InvalidDescription,
    InvalidSourceName,
    InvalidRevisionLabel,
    RevisionNotJsSafe,
    // 源路径与内容（422）
    SourceNotFound,
    SourcePathInvalid,
    SourceOutsideAllowedRoot,
    SourceNotDirectory,
    SourceEmpty,
    SourceTooLarge,
    SourceUnreadable,
    UnsafePathComponent,
    UnsafeSymlink,
    UnsafeFileType,
    UnsafeHardlink,
    UnsafeInternalMount,
    UntrustedSourceOwner,
    SourceNotApproved,
    UnsupportedManifestFormat,
    ManifestInvalid,
    ManifestDigestMismatch,
    ManifestEntryMismatch,
    // 状态与并发冲突（409）
    ReferenceRevisionConflict,
    RevisionExhausted,
    SourceNotReady,
    SourceIdentityChanged,
    SourceInUse,
    EnvironmentNotReady,
    EnvironmentBusy,
    EnvironmentSuperseded,
    EnvironmentApplyFailed,
    ManagementUnauthorized,
    // 后端能力（503）
    RecursiveReadOnlyUnsupported,
    ReferenceStorageUnavailable,
    // 内部
    EnvironmentRecoveryUnavailable,
    OutputLimitExceeded,
    InternalError,
}

impl ReferenceErrorCode {
    /// 统一错误契约建议使用的 HTTP 状态码。
    pub fn http_status(&self) -> u16 {
        match self {
            Self::InvalidRequest
            | Self::InvalidAlias
            | Self::DuplicateAlias
            | Self::DuplicateBindingId
            | Self::DuplicateSourceBinding
            | Self::TooManyBindings
            | Self::InvalidDescription
            | Self::InvalidSourceName
            | Self::InvalidRevisionLabel
            | Self::RevisionNotJsSafe
            | Self::SourceNotFound
            | Self::SourcePathInvalid
            | Self::SourceOutsideAllowedRoot
            | Self::SourceNotDirectory
            | Self::SourceEmpty
            | Self::SourceTooLarge
            | Self::SourceUnreadable
            | Self::UnsafePathComponent
            | Self::UnsafeSymlink
            | Self::UnsafeFileType
            | Self::UnsafeHardlink
            | Self::UnsafeInternalMount
            | Self::UntrustedSourceOwner
            | Self::SourceNotApproved
            | Self::UnsupportedManifestFormat
            | Self::ManifestInvalid
            | Self::ManifestDigestMismatch
            | Self::ManifestEntryMismatch => 422,
            Self::ReferenceRevisionConflict
            | Self::RevisionExhausted
            | Self::SourceNotReady
            | Self::SourceIdentityChanged
            | Self::SourceInUse
            | Self::EnvironmentNotReady
            | Self::EnvironmentBusy
            | Self::EnvironmentSuperseded
            | Self::EnvironmentApplyFailed => 409,
            Self::ManagementUnauthorized => 401,
            Self::RecursiveReadOnlyUnsupported | Self::ReferenceStorageUnavailable => 503,
            Self::OutputLimitExceeded => 413,
            Self::EnvironmentRecoveryUnavailable | Self::InternalError => 500,
        }
    }

    /// 同一请求在条件不变时是否可能成功；重试由上层结合操作收据决定。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::EnvironmentBusy
                | Self::SourceNotReady
                | Self::RecursiveReadOnlyUnsupported
                | Self::ReferenceStorageUnavailable
                | Self::InternalError
        )
    }
}

/// 字段级错误；`field` 使用请求体的 JSON 路径风格命名。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceFieldError {
    pub field: String,
    pub message: String,
}

/// 统一错误响应：稳定机器码、中文消息、字段错误、请求 ID 与重试建议。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceErrorResponse {
    pub code: ReferenceErrorCode,
    pub message: String,
    #[serde(default)]
    pub field_errors: Vec<ReferenceFieldError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub retryable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn binding() -> serde_json::Value {
        json!({
            "id": "8f058410-c69e-45d6-a10d-d204db72b30a",
            "source_id": "c97b5485-5224-470f-81f7-85ce77297c02",
            "alias": "linux",
            "description": "参考 IRQ 与驱动生命周期",
            "enabled": true
        })
    }

    #[test]
    fn replace_request_rejects_missing_and_null_bindings() {
        assert!(
            serde_json::from_value::<ReplaceProjectReferencesRequest>(json!({
                "expected_revision": 0
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ReplaceProjectReferencesRequest>(json!({
                "expected_revision": 0,
                "bindings": null
            }))
            .is_err()
        );

        let empty: ReplaceProjectReferencesRequest =
            serde_json::from_value(json!({ "expected_revision": 3, "bindings": [] }))
                .expect("清空配置必须是合法请求");
        assert_eq!(empty.expected_revision, 3);
        assert!(empty.bindings.is_empty());
    }

    #[test]
    fn reference_dtos_reject_unknown_fields() {
        assert!(
            serde_json::from_value::<ReplaceProjectReferencesRequest>(json!({
                "expected_revision": 0,
                "bindings": [],
                "access": "rw"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ProjectReferenceBinding>(json!({
                "id": "8f058410-c69e-45d6-a10d-d204db72b30a",
                "source_id": "c97b5485-5224-470f-81f7-85ce77297c02",
                "alias": "linux",
                "description": "",
                "enabled": true,
                "read_only": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<NewReferenceSourceRequest>(json!({
                "name": "Linux 参考",
                "host_path": "/srv/mai-references/linux/repo",
                "revision_label": "v6.12",
                "approved_for_agent_access": true,
                "verified_commit_sha": "deadbeef"
            }))
            .is_err()
        );
    }

    #[test]
    fn source_provenance_requires_git_identifiers_only_for_git_tree() {
        let git: SourceProvenance = serde_json::from_value(json!({
            "kind": "git_tree",
            "commit_sha": "f635d71a4fc1f3822a107295e98ca717c0f724fa",
            "tree_sha": "9e56ed9f6d1b0a4ea2d2cf1c3f1c5f9b1d3a2c40"
        }))
        .expect("git 来源");
        assert_eq!(SourceProvenanceKind::GitTree, git.kind);
        assert!(git.commit_sha.is_some());

        let directory =
            serde_json::to_value(SourceProvenance::directory_snapshot()).expect("序列化目录来源");
        assert_eq!(json!({ "kind": "directory_snapshot" }), directory);

        // 未知字段仍然拒绝；`commit_sha`/`tree_sha` 是已知字段，"来源种类与标识一致"
        // 由 mai-references 的验证与重新验证负责（篡改记录会在重新验证时被判为身份变化）。
        assert!(
            serde_json::from_value::<SourceProvenance>(json!({
                "kind": "git_tree",
                "commit_sha": "f635d71a4fc1f3822a107295e98ca717c0f724fa",
                "tree_sha": "9e56ed9f6d1b0a4ea2d2cf1c3f1c5f9b1d3a2c40",
                "verified": true
            }))
            .is_err()
        );
    }

    #[test]
    fn binding_round_trips_with_snake_case_enums() {
        let value = binding();
        let decoded: ProjectReferenceBinding =
            serde_json::from_value(value.clone()).expect("绑定反序列化");
        assert_eq!(
            ProjectReferenceConfiguration {
                revision: 1,
                bindings: vec![decoded],
            }
            .bindings
            .len(),
            1
        );

        assert_eq!(
            serde_json::to_value(ReferenceApplyPolicy::StopAndApply).expect("策略序列化"),
            json!("stop_and_apply")
        );
        assert_eq!(
            serde_json::to_value(ReferenceApplyPolicy::default()).expect("默认策略"),
            json!("when_idle")
        );
        assert_eq!(
            serde_json::to_value(ReferenceApplyPhase::CleanupPending).expect("阶段序列化"),
            json!("cleanup_pending")
        );
        assert_eq!(
            serde_json::to_value(ReferenceApplicationState::NoExecutionEnvironment)
                .expect("应用状态序列化"),
            json!("no_execution_environment")
        );
    }

    #[test]
    fn apply_request_uses_when_idle_when_policy_missing() {
        let request: ApplyProjectReferencesRequest =
            serde_json::from_value(json!({ "expected_revision": 4 })).expect("应用请求");
        assert_eq!(ReferenceApplyPolicy::WhenIdle, request.policy);
    }

    #[test]
    fn revision_bounds_are_js_safe() {
        assert_eq!(MAX_JS_SAFE_INTEGER, 9_007_199_254_740_991);
        let snapshot = AppliedReferenceSnapshot {
            project_id: Uuid::nil(),
            configuration_revision: MAX_JS_SAFE_INTEGER,
            fingerprint: "sha256:0".to_string(),
            references: Vec::new(),
            observed_at: Utc::now(),
        };
        let value = serde_json::to_value(&snapshot).expect("快照序列化");
        assert_eq!(json!(MAX_JS_SAFE_INTEGER), value["configuration_revision"]);
        assert!(snapshot.is_empty());
    }

    #[test]
    fn status_query_normalizes_limit() {
        assert_eq!(
            ReferenceStatusQuery {
                offset: 0,
                limit: 0
            }
            .normalized()
            .limit,
            1
        );
        assert_eq!(
            ReferenceStatusQuery {
                offset: 0,
                limit: u64::MAX
            }
            .normalized()
            .limit,
            MAX_REFERENCE_STATUS_LIMIT
        );
        assert_eq!(
            DEFAULT_REFERENCE_STATUS_LIMIT,
            ReferenceStatusQuery::default().limit
        );
    }

    #[test]
    fn patch_request_defaults_to_no_change() {
        let patch: PatchReferenceSourceRequest =
            serde_json::from_value(json!({})).expect("空 PATCH");
        assert!(patch.is_empty());
        assert_eq!(
            json!({ "disabled": true }),
            serde_json::to_value(PatchReferenceSourceRequest {
                disabled: Some(true),
                ..Default::default()
            })
            .expect("PATCH 序列化")
        );
    }

    #[test]
    fn error_response_shape_is_stable() {
        let response = ReferenceErrorResponse {
            code: ReferenceErrorCode::ReferenceRevisionConflict,
            message: "配置版本已变化，请刷新后重试".to_string(),
            field_errors: vec![ReferenceFieldError {
                field: "expected_revision".to_string(),
                message: "期望版本与当前版本不一致".to_string(),
            }],
            request_id: Some("req-1".to_string()),
            retryable: false,
        };
        assert_eq!(409, response.code.http_status());
        assert_eq!(
            json!({
                "code": "reference_revision_conflict",
                "message": "配置版本已变化，请刷新后重试",
                "field_errors": [{
                    "field": "expected_revision",
                    "message": "期望版本与当前版本不一致"
                }],
                "request_id": "req-1",
                "retryable": false
            }),
            serde_json::to_value(&response).expect("错误响应序列化")
        );
    }

    #[test]
    fn error_codes_expose_stable_status_mapping() {
        assert_eq!(422, ReferenceErrorCode::UnsafeSymlink.http_status());
        assert_eq!(409, ReferenceErrorCode::SourceInUse.http_status());
        assert_eq!(
            503,
            ReferenceErrorCode::RecursiveReadOnlyUnsupported.http_status()
        );
        assert_eq!(
            401,
            ReferenceErrorCode::ManagementUnauthorized.http_status()
        );
        assert_eq!(500, ReferenceErrorCode::InternalError.http_status());
        assert!(ReferenceErrorCode::ReferenceStorageUnavailable.is_retryable());
        assert!(!ReferenceErrorCode::InvalidAlias.is_retryable());
    }

    #[test]
    fn applied_reference_snapshot_keeps_host_path_out_of_model_projection() {
        let applied = AppliedReference {
            binding_id: Uuid::nil(),
            source_id: Uuid::nil(),
            name: "Linux".to_string(),
            revision_label: "v6.12".to_string(),
            version: "f635d71a4fc1f3822a107295e98ca717c0f724fa".to_string(),
            alias: "linux".to_string(),
            container_path: "/references/linux".to_string(),
            host_path: "/srv/mai-references/linux/repo".to_string(),
            tree_digest: "sha256:0".to_string(),
            provenance: SourceProvenance::git_tree("a".repeat(40), "b".repeat(40)),
            source_device: 2049,
            source_inode: 42,
            description: "参考内核".to_string(),
            access: ReferenceAccess::ReadOnly,
        };
        let json = serde_json::to_value(&applied).expect("已应用引用序列化");
        assert_eq!(json!("/srv/mai-references/linux/repo"), json["host_path"]);

        let visible = ModelVisibleReference {
            source_id: applied.source_id,
            name: applied.name.clone(),
            version: applied.version.clone(),
            container_path: applied.container_path.clone(),
            description: applied.description.clone(),
            access: ReferenceAccess::ReadOnly,
        };
        let visible = serde_json::to_value(&visible).expect("模型可见投影序列化");
        assert!(visible.get("host_path").is_none());
        assert!(visible.get("source_device").is_none());
        assert_eq!(json!("/references/linux"), visible["container_path"]);
    }
}
