/**
 * 参考源码绑定表单的纯函数：规范化、校验、摘要。
 *
 * 这里不触碰 React、Query 或网络，因此可以在不挂载组件的情况下验证 alias 规则、
 * 重复检测与"晚到响应只匹配当前草稿"的摘要计算。
 */

import {
  ALIAS_PATTERN,
  DEFAULT_MAX_BINDINGS_PER_PROJECT,
  MAX_ALIAS_CHARS,
  MAX_DESCRIPTION_CHARS,
  MAX_REVISION_LABEL_CHARS,
  MAX_SOURCE_NAME_CHARS,
  REFERENCE_CONTAINER_ROOT,
  type ProjectReferenceBinding,
} from "@/api/reference-types"

/** 校验限制；缺省时使用协议常量。 */
export interface ReferenceLimits {
  maxBindings: number
  maxAliasChars: number
  maxDescriptionChars: number
  maxSourceNameChars: number
  maxRevisionLabelChars: number
  aliasPattern: string
}

export const defaultReferenceLimits: ReferenceLimits = {
  maxBindings: DEFAULT_MAX_BINDINGS_PER_PROJECT,
  maxAliasChars: MAX_ALIAS_CHARS,
  maxDescriptionChars: MAX_DESCRIPTION_CHARS,
  maxSourceNameChars: MAX_SOURCE_NAME_CHARS,
  maxRevisionLabelChars: MAX_REVISION_LABEL_CHARS,
  aliasPattern: ALIAS_PATTERN,
}

/** 单个绑定行的字段错误；缺失的键表示该字段没有错误。 */
export interface BindingRowErrors {
  source?: string
  alias?: string
  description?: string
}

/** 绑定表整体校验结果。 */
export interface BindingsValidation {
  /** 以 binding.id 为键的行错误，便于按稳定 key 渲染。 */
  byId: Record<string, BindingRowErrors>
  /** 与具体行无关的错误（例如超过上限）。 */
  general: string[]
  valid: boolean
}

/** 参考源登记表单的字段错误。 */
export interface SourceDraftErrors {
  name?: string
  host_path?: string
  revision_label?: string
  approved_for_agent_access?: string
}

/** 参考源登记表单的草稿字段。 */
export interface ReferenceSourceDraft {
  name: string
  host_path: string
  revision_label: string
  approved_for_agent_access: boolean
}

let bindingIdCounter = 0

/** 生成绑定条目的稳定 ID；优先使用 crypto.randomUUID。 */
export function newBindingId(): string {
  const globalCrypto: Crypto | undefined = typeof crypto === "undefined" ? undefined : crypto
  if (globalCrypto && typeof globalCrypto.randomUUID === "function") return globalCrypto.randomUUID()
  bindingIdCounter += 1
  return `draft-${bindingIdCounter}-${Math.random().toString(16).slice(2, 10)}`
}

/** 规范化的容器内挂载目标：只由 alias 派生，不接受用户输入。 */
export function aliasContainerPath(alias: string): string {
  const trimmed = alias.trim()
  return trimmed ? `${REFERENCE_CONTAINER_ROOT}/${trimmed}` : `${REFERENCE_CONTAINER_ROOT}/`
}

function aliasPatternOf(limits: ReferenceLimits): RegExp {
  try {
    return new RegExp(limits.aliasPattern)
  } catch {
    return new RegExp(ALIAS_PATTERN)
  }
}

/** 归一化绑定：裁剪首尾空白，保持顺序与 ID。 */
export function normalizeBindings(bindings: ProjectReferenceBinding[]): ProjectReferenceBinding[] {
  return bindings.map((binding) => ({
    id: binding.id,
    source_id: binding.source_id,
    alias: binding.alias.trim(),
    description: binding.description.trim(),
    enabled: binding.enabled,
  }))
}

/** 两个绑定条目在规范化后是否完全相同。 */
export function sameBinding(a: ProjectReferenceBinding, b: ProjectReferenceBinding): boolean {
  return a.id === b.id
    && a.source_id === b.source_id
    && a.alias.trim() === b.alias.trim()
    && a.description.trim() === b.description.trim()
    && a.enabled === b.enabled
}

/**
 * 两组绑定在规范化后是否相同。
 *
 * 顺序视为语义的一部分（用户显式调整行序即视为改动），因此不排序比较。
 */
export function sameBindings(a: ProjectReferenceBinding[], b: ProjectReferenceBinding[]): boolean {
  if (a.length !== b.length) return false
  return a.every((binding, index) => sameBinding(binding, b[index]))
}

/**
 * 草稿摘要：按 ID 排序后序列化，供"晚到结果只应用于匹配草稿"比较。
 *
 * 顺序不参与摘要，因为服务端返回的规范化配置可能重排条目。
 */
export function draftDigest(bindings: ProjectReferenceBinding[]): string {
  const normalized = normalizeBindings(bindings)
    .map((binding) => [binding.id, binding.source_id, binding.alias, binding.description, binding.enabled ? 1 : 0] as const)
    .sort((left, right) => (left[0] < right[0] ? -1 : left[0] > right[0] ? 1 : 0))
  return JSON.stringify(normalized)
}

/** 字符数按 Unicode 码点统计，与服务端"最多 N 个 Unicode 字符"的约束一致。 */
export function unicodeLength(value: string): number {
  return Array.from(value).length
}

/** 校验绑定表：alias 规则、重复、上限与说明长度。 */
export function validateBindings(
  bindings: ProjectReferenceBinding[],
  limits: ReferenceLimits = defaultReferenceLimits,
): BindingsValidation {
  const pattern = aliasPatternOf(limits)
  const byId: Record<string, BindingRowErrors> = {}
  const general: string[] = []
  const aliasCounts = new Map<string, number>()
  const sourceCounts = new Map<string, number>()
  const idCounts = new Map<string, number>()

  for (const binding of bindings) {
    const alias = binding.alias.trim()
    aliasCounts.set(alias, (aliasCounts.get(alias) ?? 0) + 1)
    sourceCounts.set(binding.source_id, (sourceCounts.get(binding.source_id) ?? 0) + 1)
    idCounts.set(binding.id, (idCounts.get(binding.id) ?? 0) + 1)
  }

  for (const binding of bindings) {
    const errors: BindingRowErrors = {}
    const alias = binding.alias.trim()
    if (!binding.source_id) {
      errors.source = "选择要绑定的参考源。"
    } else if ((sourceCounts.get(binding.source_id) ?? 0) > 1) {
      errors.source = "同一参考源在同一项目内只能绑定一次。"
    }
    if (!alias) {
      errors.alias = "别名不能为空。"
    } else if (unicodeLength(alias) > limits.maxAliasChars || !pattern.test(alias)) {
      errors.alias = `别名需匹配 ${limits.aliasPattern}，且不超过 ${limits.maxAliasChars} 个字符。`
    } else if ((aliasCounts.get(alias) ?? 0) > 1) {
      errors.alias = "同一项目内别名必须唯一（含禁用条目）。"
    }
    if (unicodeLength(binding.description) > limits.maxDescriptionChars) {
      errors.description = `说明不超过 ${limits.maxDescriptionChars} 个字符。`
    }
    if (Object.keys(errors).length > 0) byId[binding.id] = errors
  }

  if (bindings.length > limits.maxBindings) {
    general.push(`单个项目最多 ${limits.maxBindings} 条绑定（含禁用条目）。`)
  }
  for (const [id, count] of idCounts) {
    if (count > 1) byId[id] = { ...byId[id], alias: byId[id]?.alias ?? "绑定条目 ID 重复。" }
  }

  return { byId, general, valid: general.length === 0 && Object.keys(byId).length === 0 }
}

/** 校验参考源登记表单；approved 必须由管理员显式确认为 true。 */
export function validateSourceDraft(
  draft: ReferenceSourceDraft,
  limits: ReferenceLimits = defaultReferenceLimits,
): SourceDraftErrors {
  const errors: SourceDraftErrors = {}
  const name = draft.name.trim()
  const hostPath = draft.host_path.trim()
  const revisionLabel = draft.revision_label.trim()

  if (!name) {
    errors.name = "名称不能为空。"
  } else if (unicodeLength(name) > limits.maxSourceNameChars) {
    errors.name = `名称不超过 ${limits.maxSourceNameChars} 个字符。`
  }
  if (!hostPath) {
    errors.host_path = "填写运行 Docker 的服务器上的绝对目录。"
  } else if (!hostPath.startsWith("/")) {
    errors.host_path = "必须是以 / 开头的服务器绝对路径。"
  }
  if (!revisionLabel) {
    errors.revision_label = "版本说明不能为空。"
  } else if (unicodeLength(revisionLabel) > limits.maxRevisionLabelChars) {
    errors.revision_label = `版本说明不超过 ${limits.maxRevisionLabelChars} 个字符。`
  }
  if (!draft.approved_for_agent_access) {
    errors.approved_for_agent_access = "需要确认该目录适合提供给 Agent 与模型访问。"
  }
  return errors
}

/** 登记表单字段错误是否为空。 */
export function sourceDraftIsValid(errors: SourceDraftErrors): boolean {
  return Object.keys(errors).length === 0
}
