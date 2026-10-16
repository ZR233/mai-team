/**
 * 项目参考源码的草稿存储。
 *
 * 设计要点：
 * - 草稿按 `projectId` 拥有，切换项目时保留在内存中，绝不把 A 的草稿套用到 B。
 * - `original`（最近一次服务器/保存基线的规范化绑定）与 `draft`（用户编辑）分离；
 *   服务器事实的刷新只更新基线，不重置仍在编辑中的草稿。
 * - 每个异步操作带有请求序号；晚到的响应只更新它所属的那次操作，不覆盖更新后的草稿。
 * - 保存期间允许继续编辑；保存成功后用服务器返回值建立新基线，并保留用户在飞行中
 *   追加的编辑。
 *
 * 该 store 位于 React 之外（模块级单例），因此 Query 的失效、SSE 事件与组件卸载都不会
 * 破坏未提交的用户输入。
 */

import type { ProjectReferenceBinding, ProjectReferenceConfiguration } from "@/api/reference-types"

import { draftDigest, normalizeBindings, sameBindings } from "./reference-form"

/** 保存冲突：服务端已有更新的 durable 配置。 */
export interface ReferenceDraftConflict {
  /** 本次保存使用的 expected_revision。 */
  expectedRevision: number
  /** 服务端当前版本；未知时为 null（由 UI 结合最新查询数据显示）。 */
  serverRevision: number | null
  message: string
  requestId: string | null
}

/** 单个项目的参考源码编辑草稿。 */
export interface ReferenceDraft {
  projectId: string
  /** 基线修订号；保存时作为 CAS 的 expected_revision。 */
  baseRevision: number
  /** 最近一次已知的服务器/保存基线（规范化后）。 */
  original: ProjectReferenceBinding[]
  /** 用户当前编辑内容。 */
  draft: ProjectReferenceBinding[]
  /** `draft` 的摘要，用于匹配晚到的验证结果。 */
  digest: string
  /** 是否有保存请求正在进行。 */
  saving: boolean
  /** 最近一次失败的人类可读消息；保存成功后清空。 */
  error: string | null
  /** 最近一次 CAS 冲突；由用户显式选择后清除。 */
  conflict: ReferenceDraftConflict | null
  /** 请求序号；每次发起保存自增，用于丢弃晚到的响应。 */
  sequence: number
}

function draftFromServer(projectId: string, configuration: ProjectReferenceConfiguration): ReferenceDraft {
  const bindings = normalizeBindings(configuration.bindings)
  return {
    projectId,
    baseRevision: configuration.revision,
    original: bindings,
    draft: bindings,
    digest: draftDigest(bindings),
    saving: false,
    error: null,
    conflict: null,
    sequence: 0,
  }
}

/** 草稿是否相对基线有未提交改动。 */
export function isDraftDirty(draft: ReferenceDraft): boolean {
  return !sameBindings(draft.original, draft.draft)
}

/**
 * 按项目 ID 拥有草稿的存储。
 *
 * 默认导出的单例服务于应用运行时；测试可以创建独立实例，避免用例之间互相污染。
 */
export class ReferenceDraftStore {
  private readonly drafts = new Map<string, ReferenceDraft>()
  private readonly projectListeners = new Map<string, Set<() => void>>()
  private readonly globalListeners = new Set<() => void>()

  /** 订阅某个项目的草稿变化。 */
  subscribe(projectId: string, listener: () => void): () => void {
    const listeners = this.projectListeners.get(projectId) ?? new Set<() => void>()
    listeners.add(listener)
    this.projectListeners.set(projectId, listeners)
    return () => {
      listeners.delete(listener)
      if (listeners.size === 0) this.projectListeners.delete(projectId)
    }
  }

  /** 订阅任意项目的草稿变化（用于导航守卫）。 */
  subscribeAll(listener: () => void): () => void {
    this.globalListeners.add(listener)
    return () => {
      this.globalListeners.delete(listener)
    }
  }

  /** 读取草稿；尚未初始化时返回 undefined。 */
  peek(projectId: string): ReferenceDraft | undefined {
    return this.drafts.get(projectId)
  }

  /** 是否存在未保存的草稿改动。 */
  hasDirtyDraft(projectId: string): boolean {
    const draft = this.drafts.get(projectId)
    return draft ? isDraftDirty(draft) : false
  }

  /** 是否任意项目存在未保存的草稿改动。 */
  hasAnyDirtyDraft(): boolean {
    for (const draft of this.drafts.values()) {
      if (isDraftDirty(draft)) return true
    }
    return false
  }

  /** 首次见到某个项目时建立草稿；已有草稿时不做任何事。 */
  ensureFromServer(projectId: string, configuration: ProjectReferenceConfiguration | null | undefined): void {
    if (!configuration || this.drafts.has(projectId)) return
    // 建立初值时不需要通知订阅者：此刻组件尚未订阅，且初值 draft === original 一定不是 dirty。
    this.write(projectId, draftFromServer(projectId, configuration), false)
  }

  /**
   * 用服务器事实同步草稿。
   *
   * 只在本地没有未提交改动、且服务器修订号不比已知基线更旧时才采用服务器内容；
   * 否则保留用户草稿（CAS 冲突由保存失败路径处理）。
   */
  syncFromServer(projectId: string, configuration: ProjectReferenceConfiguration): void {
    const existing = this.drafts.get(projectId)
    if (!existing) {
      this.write(projectId, draftFromServer(projectId, configuration))
      return
    }
    // 保存进行中不改动基线：该请求完成后会用服务器返回值建立新基线。
    if (existing.saving) return
    if (isDraftDirty(existing)) return
    if (configuration.revision < existing.baseRevision) return
    // 服务器事实没有变化时保持同一个快照对象：避免无意义的重渲染，也让卸载后重开拿到稳定快照。
    if (existing.baseRevision === configuration.revision && sameBindings(existing.original, configuration.bindings)) return
    this.write(projectId, { ...draftFromServer(projectId, configuration), sequence: existing.sequence })
  }

  /** 用户编辑：整体替换草稿绑定。 */
  setBindings(projectId: string, bindings: ProjectReferenceBinding[]): void {
    const existing = this.drafts.get(projectId)
    if (!existing) return
    const normalized = normalizeBindings(bindings)
    this.write(projectId, { ...existing, draft: normalized, digest: draftDigest(normalized) })
  }

  /**
   * 开始一次保存。
   *
   * 返回本次请求的序号；若已有保存在进行中则返回 null（双击只提交一次）。
   */
  beginSave(projectId: string): number | null {
    const existing = this.drafts.get(projectId)
    if (!existing || existing.saving) return null
    const sequence = existing.sequence + 1
    this.write(projectId, { ...existing, saving: true, error: null, conflict: null, sequence })
    return sequence
  }

  /** 保存成功：用服务器返回值建立新基线，保留编辑期间追加的改动。 */
  saveSucceeded(projectId: string, sequence: number, configuration: ProjectReferenceConfiguration): void {
    const existing = this.drafts.get(projectId)
    if (!existing || existing.sequence !== sequence) return
    const merged = normalizeBindings(existing.draft)
    const baseline = normalizeBindings(configuration.bindings)
    this.write(projectId, {
      ...existing,
      baseRevision: configuration.revision,
      original: baseline,
      draft: merged,
      digest: draftDigest(merged),
      saving: false,
      error: null,
      conflict: null,
    })
  }

  /** 保存失败：保留草稿；CAS 冲突额外记录服务端版本信息。 */
  saveFailed(projectId: string, sequence: number, failure: { message: string; conflict?: ReferenceDraftConflict | null }): void {
    const existing = this.drafts.get(projectId)
    if (!existing || existing.sequence !== sequence) return
    this.write(projectId, {
      ...existing,
      saving: false,
      error: failure.message,
      conflict: failure.conflict ?? null,
    })
  }

  /** 放弃本地草稿，整体采用服务器版本。 */
  resetToServer(projectId: string, configuration: ProjectReferenceConfiguration): void {
    const existing = this.drafts.get(projectId)
    if (!existing) {
      this.write(projectId, draftFromServer(projectId, configuration))
      return
    }
    this.write(projectId, { ...draftFromServer(projectId, configuration), sequence: existing.sequence })
  }

  /** 用户选择留着自己的草稿继续编辑：仅关闭冲突提示。 */
  dismissConflict(projectId: string): void {
    const existing = this.drafts.get(projectId)
    if (!existing || !existing.conflict) return
    this.write(projectId, { ...existing, conflict: null })
  }

  /** 清空所有草稿；仅供测试隔离使用。 */
  resetAll(): void {
    this.drafts.clear()
    this.notify(null)
  }

  private write(projectId: string, draft: ReferenceDraft, notify = true): void {
    this.drafts.set(projectId, draft)
    if (notify) this.notify(projectId)
  }

  private notify(projectId: string | null): void {
    if (projectId) {
      for (const listener of this.projectListeners.get(projectId) ?? []) listener()
    }
    for (const listener of this.globalListeners) listener()
  }
}

/** 应用运行时共享的草稿存储。 */
export const referenceDraftStore = new ReferenceDraftStore()
