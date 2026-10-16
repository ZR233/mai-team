/**
 * 项目参考源码的精确 TypeScript DTO。
 *
 * 这里是 `mai-protocol/src/references.rs` 的前端镜像：字段名、可空性、枚举取值都逐一
 * 对齐 Rust 定义，不使用 `any`，也不依赖 ProjectSummary 的 index signature。服务端用
 * `deny_unknown_fields` 校验请求体，因此本文件同样只声明真实存在的字段，前端不得凭空
 * 增加服务端不接受的键（例如 `verified_commit_sha`、`access`）。
 */

/** 容器内参考源码的保留命名空间；项目与绑定都不能自定义挂载目标。 */
export const REFERENCE_CONTAINER_ROOT = "/references"

/** alias 匹配规则：小写字母开头，其余仅小写字母/数字/下划线/连字符，总长不超过 32。 */
export const ALIAS_PATTERN = "^[a-z][a-z0-9_-]{0,31}$"

/** alias 允许的最大字符数。 */
export const MAX_ALIAS_CHARS = 32

/** 单项目默认最多绑定条数（包含禁用条目）。 */
export const DEFAULT_MAX_BINDINGS_PER_PROJECT = 16

/** 绑定说明允许的最大 Unicode 字符数。 */
export const MAX_DESCRIPTION_CHARS = 512

/** 参考源名称允许的最大 Unicode 字符数。 */
export const MAX_SOURCE_NAME_CHARS = 128

/** 版本标签允许的最大 Unicode 字符数。 */
export const MAX_REVISION_LABEL_CHARS = 256

/** JS 安全整数上限（2^53 - 1）；配置修订号必须落在该范围内。 */
export const MAX_JS_SAFE_INTEGER = 9_007_199_254_740_991

/** 参考源登记的验证生命周期。只有 `ready` 的源可以被项目启用。 */
export type SourceState = "validating" | "ready" | "invalid" | "disabled" | "missing"

/** 发布来源的种类。 */
export type SourceProvenanceKind = "git_tree" | "directory_snapshot"

/**
 * 参考源的发布来源。
 *
 * `git_tree` 必须同时携带 `commit_sha` 与 `tree_sha`；`directory_snapshot` 不携带任何
 * Git 标识。字段可缺席（Rust 侧 `skip_serializing_if` 会省略）。
 */
export interface SourceProvenance {
  kind: SourceProvenanceKind
  commit_sha?: string | null
  tree_sha?: string | null
}

/** 一次验证失败的结构化记录；人类消息与稳定机器码分离。 */
export interface ReferenceValidationFailure {
  code: ReferenceErrorCode
  message: string
  path?: string | null
  observed_at: string
}

/** 管理面看到的参考源登记项。`host_path` 只用于管理 UI，不进模型上下文。 */
export interface ReferenceSource {
  id: string
  name: string
  revision_label: string
  host_path: string
  provenance: SourceProvenance
  manifest_digest?: string | null
  state: SourceState
  approved_for_agent_access: boolean
  file_count: number
  byte_count: number
  source_device: number
  source_inode: number
  validation_error?: ReferenceValidationFailure | null
  warnings: string[]
  created_at: string
  updated_at: string
}

/**
 * 登记新参考源的请求。
 *
 * 请求只给出宿主路径；版本来源与摘要由服务端验证器读取 manifest 后写入，前端不得
 * 声明 `verified_commit_sha` 之类的字段。
 */
export interface NewReferenceSourceRequest {
  name: string
  host_path: string
  revision_label: string
  approved_for_agent_access: boolean
}

/** 修改参考源元数据的请求；路径、来源与摘要都不在可改字段内。 */
export interface PatchReferenceSourceRequest {
  name?: string
  revision_label?: string
  approved_for_agent_access?: boolean
  /** 置为 `true` 表示全局禁用；服务端仍要求没有任何项目绑定。 */
  disabled?: boolean
}

/** 项目对某个固定版本参考源的绑定条目。`id` 是稳定条目 ID，同时是 UI key。 */
export interface ProjectReferenceBinding {
  id: string
  source_id: string
  alias: string
  description: string
  enabled: boolean
}

/**
 * 项目的规范化参考源配置。
 *
 * 这是 `GET /projects/{id}/references` 与 `PUT /projects/{id}/references` 的唯一响应形状；
 * 各 Agent 的应用事实（counts/agents/operation）只出现在 `ProjectReferenceStatus`。
 */
export interface ProjectReferenceConfiguration {
  revision: number
  bindings: ProjectReferenceBinding[]
}

/**
 * 整体替换项目参考源配置的请求（CAS）。
 *
 * `bindings` 必须显式给出：缺失或 `null` 都是请求错误，`[]` 才表示清空。
 */
export interface ReplaceProjectReferencesRequest {
  expected_revision: number
  bindings: ProjectReferenceBinding[]
}

/** 应用策略：默认等当前 turn 结束，或由管理员确认后停止再应用。 */
export type ReferenceApplyPolicy = "when_idle" | "stop_and_apply"

/** 申请立即重试/停止并应用某个已经 durable 的配置版本。 */
export interface ApplyProjectReferencesRequest {
  expected_revision: number
  policy?: ReferenceApplyPolicy
}

/** 单个 Agent 的换代阶段。 */
export type ReferenceApplyPhase =
  | "waiting_idle"
  | "draining"
  | "snapshotting"
  | "creating"
  | "verifying"
  | "rebinding"
  | "publishing"
  | "cleanup_pending"
  | "succeeded"
  | "failed"
  | "superseded"

/** 参考目录的挂载访问方式；目前只有只读。 */
export type ReferenceAccess = "read_only"

/** 一次应用操作的可恢复收据。 */
export interface ReferenceApplyOperation {
  id: string
  agent_id: string
  project_id: string
  phase: ReferenceApplyPhase
  policy: ReferenceApplyPolicy
  target_revision: number
  old_container_id?: string | null
  candidate_container_id?: string | null
  snapshot_image_id?: string | null
  failure?: ReferenceValidationFailure | null
  created_at: string
  updated_at: string
}

/**
 * 已经应用到某个具体 generation 的单个参考源。
 *
 * `host_path` 供 Docker 生成 bind 参数使用，只存在于服务端快照，不进模型上下文。
 */
export interface AppliedReference {
  binding_id: string
  source_id: string
  name: string
  revision_label: string
  /** 版本标识：Git 快照为完整 commit SHA，普通目录为目录摘要。 */
  version: string
  alias: string
  container_path: string
  host_path: string
  tree_digest: string
  provenance: SourceProvenance
  source_device: number
  source_inode: number
  description: string
  access: ReferenceAccess
}

/** 某个 generation 实际生效的参考源集合；只包含 enabled 条目。 */
export interface AppliedReferenceSnapshot {
  project_id: string
  configuration_revision: number
  fingerprint: string
  references: AppliedReference[]
  observed_at: string
}

/** 单个 Agent 的应用状态。 */
export type ReferenceApplicationState =
  | "applied"
  | "pending"
  | "applying"
  | "waiting_idle"
  | "failed"
  | "deferred"
  | "no_execution_environment"

/** 单个 Agent 的参考源应用状态投影。 */
export interface AgentReferenceApplication {
  agent_id: string
  project_id: string
  generation?: string | null
  container_id?: string | null
  effective_image_id?: string | null
  applied_revision: number
  applied_fingerprint?: string | null
  state: ReferenceApplicationState
  operation?: ReferenceApplyOperation | null
  failure?: ReferenceValidationFailure | null
  observed_at: string
}

/** 项目级应用计数；统计完整集合，不受分页限制。 */
export interface ReferenceApplicationCounts {
  applied: number
  waiting: number
  failed: number
  deferred: number
  no_environment: number
}

/** 项目级应用聚合状态。 */
export type ProjectReferenceApplicationState = "pending" | "applied" | "failed" | "deferred"

/** Agent 列表的分页信息。 */
export interface ReferencePageInfo {
  offset: number
  limit: number
  /** 完整集合条数，不等于本页条数。 */
  total: number
}

/**
 * 项目参考源状态响应：durable 配置 + 实际应用状态。
 *
 * 该形状由 `GET /projects/{id}/references/status` 与 `POST /projects/{id}/references/apply`
 * （202）返回；`GET/PUT /projects/{id}/references` 只返回 `ProjectReferenceConfiguration`。
 */
export interface ProjectReferenceStatus {
  project_id: string
  configuration: ProjectReferenceConfiguration
  target_revision: number
  state: ProjectReferenceApplicationState
  counts: ReferenceApplicationCounts
  agents: AgentReferenceApplication[]
  page: ReferencePageInfo
  observed_at: string
}

/** 部署侧参考源能力与限制；管理 UI 只能读取，不能扩大允许根。 */
export interface ReferenceSourcePolicy {
  allowed_roots: string[]
  trusted_owner_uids: number[]
  reference_container_root: string
  alias_pattern: string
  max_alias_chars: number
  max_bindings_per_project: number
  max_description_chars: number
  source_name_max_chars: number
  revision_label_max_chars: number
  manifest_file_name: string
  supported_provenance_kinds: SourceProvenanceKind[]
  /** 严格要求递归只读；为 `false` 时代表该能力尚未通过环境验证。 */
  require_strict_recursive_read_only: boolean
  minimum_docker_engine_version: string
  minimum_kernel_version: string
}

/** 参考源领域的稳定错误机器码。 */
export type ReferenceErrorCode =
  // 请求与配置校验（422）
  | "invalid_request"
  | "invalid_alias"
  | "duplicate_alias"
  | "duplicate_binding_id"
  | "duplicate_source_binding"
  | "too_many_bindings"
  | "invalid_description"
  | "invalid_source_name"
  | "invalid_revision_label"
  | "revision_not_js_safe"
  // 源路径与内容（422）
  | "source_not_found"
  | "source_path_invalid"
  | "source_outside_allowed_root"
  | "source_not_directory"
  | "source_empty"
  | "source_too_large"
  | "source_unreadable"
  | "unsafe_path_component"
  | "unsafe_symlink"
  | "unsafe_file_type"
  | "unsafe_hardlink"
  | "unsafe_internal_mount"
  | "untrusted_source_owner"
  | "source_not_approved"
  | "unsupported_manifest_format"
  | "manifest_invalid"
  | "manifest_digest_mismatch"
  | "manifest_entry_mismatch"
  // 状态与并发冲突（409）
  | "reference_revision_conflict"
  | "revision_exhausted"
  | "source_not_ready"
  | "source_identity_changed"
  | "source_in_use"
  | "environment_not_ready"
  | "environment_busy"
  | "environment_superseded"
  | "output_limit_exceeded"
  | "environment_apply_failed"
  | "management_unauthorized"
  // 后端能力（503）
  | "recursive_read_only_unsupported"
  | "reference_storage_unavailable"
  // 内部
  | "environment_recovery_unavailable"
  | "internal_error"

/** 字段级错误；`field` 使用请求体的 JSON 路径风格命名。 */
export interface ReferenceFieldError {
  field: string
  message: string
}

/** 统一错误响应：稳定机器码、中文消息、字段错误、请求 ID 与重试建议。 */
export interface ReferenceErrorResponse {
  code: ReferenceErrorCode
  message: string
  field_errors: ReferenceFieldError[]
  request_id?: string | null
  retryable: boolean
}
