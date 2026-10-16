/**
 * 把草稿存储接入 React，并提供 dirty 导航守卫。
 *
 * 组件只从这里读取草稿；服务器事实（Query/SSE）通过 `syncFromServer` 更新基线，
 * 不会覆盖未保存的编辑。
 */

import { useCallback, useEffect, useSyncExternalStore } from "react"

import type { ProjectReferenceConfiguration } from "@/api/reference-types"

import { referenceDraftStore, type ReferenceDraft } from "./reference-draft-store"

/**
 * 读取（必要时初始化）某个项目的参考源码草稿。
 *
 * 同一 `projectId` 在多次挂载之间共享同一份草稿，因此切换项目再回来仍保留未保存编辑。
 */
export function useReferenceDraft(
  projectId: string,
  configuration: ProjectReferenceConfiguration | null | undefined,
): ReferenceDraft | undefined {
  const store = referenceDraftStore
  // 幂等初始化：仅当该 projectId 还没有草稿时建立，绝不覆盖未保存编辑。
  store.ensureFromServer(projectId, configuration)
  const draft = useSyncExternalStore(
    useCallback((listener: () => void) => store.subscribe(projectId, listener), [store, projectId]),
    useCallback(() => store.peek(projectId), [store, projectId]),
    useCallback(() => store.peek(projectId), [store, projectId]),
  )
  useEffect(() => {
    if (configuration) store.syncFromServer(projectId, configuration)
  }, [store, projectId, configuration])
  return draft
}

/** 是否任意项目存在未保存的参考源码草稿。 */
export function useAnyReferenceDraftDirty(): boolean {
  const store = referenceDraftStore
  return useSyncExternalStore(
    useCallback((listener: () => void) => store.subscribeAll(listener), [store]),
    useCallback(() => store.hasAnyDirtyDraft(), [store]),
    () => false,
  )
}

/**
 * 参考源码的 dirty 导航守卫。
 *
 * `confirmLeave()` 在存在未保存草稿时弹出确认；确认后允许导航，草稿仍按项目保留在
 * 本次会话中（既不会丢弃，也不会被套用到其它项目）。同时在 dirty 时注册
 * `beforeunload`，避免整页刷新丢掉未保存输入。
 */
export function useReferenceNavigationGuard(): { hasDirtyDrafts: boolean; confirmLeave(): boolean } {
  const hasDirtyDrafts = useAnyReferenceDraftDirty()

  useEffect(() => {
    if (!hasDirtyDrafts) return
    const handler = (event: BeforeUnloadEvent) => {
      event.preventDefault()
      event.returnValue = ""
    }
    window.addEventListener("beforeunload", handler)
    return () => window.removeEventListener("beforeunload", handler)
  }, [hasDirtyDrafts])

  const confirmLeave = useCallback(() => {
    if (!hasDirtyDrafts) return true
    return window.confirm("参考源码有未保存的改动。草稿会保留在本次会话中，但尚未保存到服务器。确定离开吗？")
  }, [hasDirtyDrafts])

  return { hasDirtyDrafts, confirmLeave }
}
