/**
 * 项目参考源配置的保存驱动。
 *
 * 保存刻意不走组件内的 `useMutation`：请求生命周期由草稿存储拥有，而不是由某个已挂载
 * 的面板拥有。面板卸载（切标签页、切路由、切项目）后请求仍会完成并把结果写回存储，
 * 因此重新打开同一个项目能看到稳定快照，也不会永远停在 "Saving…"。
 */

import type { QueryClient } from "@tanstack/react-query"
import { toast } from "sonner"

import { referenceApi, referenceJsonBody, ReferenceApiError } from "@/api/reference-client"
import type { ProjectReferenceConfiguration } from "@/api/reference-types"
import { queryKeys } from "@/api/queries"

import { referenceDraftStore, type ReferenceDraftConflict } from "./reference-draft-store"
import { normalizeBindings } from "./reference-form"

/** 失效项目的 durable 配置与应用状态，不触碰草稿。 */
export async function invalidateProjectReferences(queryClient: QueryClient, projectId: string): Promise<void> {
  await Promise.all([
    queryClient.invalidateQueries({ queryKey: queryKeys.projectReferences(projectId), exact: true }),
    queryClient.invalidateQueries({ queryKey: queryKeys.projectReferencesStatus(projectId) }),
  ])
}

/**
 * 保存当前草稿并应用到项目。
 *
 * 同一项目同时只允许一个保存请求：重复调用（双击、双组件）直接返回，不产生第二个 PUT。
 * 失败保留草稿；CAS 冲突额外保留服务端版本信息供用户选择。
 */
export async function saveProjectReferences(projectId: string, queryClient: QueryClient): Promise<void> {
  const current = referenceDraftStore.peek(projectId)
  if (!current) return
  const sequence = referenceDraftStore.beginSave(projectId)
  if (sequence === null) return

  try {
    // `PUT /projects/{id}/references` 只返回 durable 配置；应用事实由 status 查询收敛。
    const saved = await referenceApi<ProjectReferenceConfiguration>(`/projects/${projectId}/references`, {
      method: "PUT",
      ...referenceJsonBody({
        expected_revision: current.baseRevision,
        bindings: normalizeBindings(current.draft),
      }),
    })
    referenceDraftStore.saveSucceeded(projectId, sequence, saved)
    await invalidateProjectReferences(queryClient, projectId)
    toast.success(`Saved revision ${saved.revision}. Applying to agent environments.`)
  } catch (error) {
    const failure = saveFailure(error, current.baseRevision)
    referenceDraftStore.saveFailed(projectId, sequence, failure)
    if (error instanceof ReferenceApiError && error.isRevisionConflict) {
      void queryClient.invalidateQueries({ queryKey: queryKeys.projectReferences(projectId) })
      toast.error(`${error.message} The server version is shown below; choose how to reconcile.`)
      return
    }
    toast.error(failure.message)
  }
}

interface SaveFailure {
  message: string
  conflict?: ReferenceDraftConflict | null
}

function saveFailure(error: unknown, expectedRevision: number): SaveFailure {
  if (error instanceof ReferenceApiError) {
    const message = error.response?.field_errors[0]?.message || error.message
    if (error.isRevisionConflict) {
      return {
        message,
        conflict: {
          expectedRevision,
          serverRevision: null,
          message,
          requestId: error.response?.request_id ?? null,
        },
      }
    }
    return { message }
  }
  return { message: error instanceof Error ? error.message : String(error) }
}
