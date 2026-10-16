/**
 * 项目 · 参考源码标签页。
 *
 * 编辑草稿按 `projectId` 拥有：Query/SSE 带来的服务器事实只更新基线，绝不重置未保存的
 * 编辑。保存使用 CAS（`expected_revision`），冲突时展示服务器当前版本由用户选择，不自动
 * 覆盖远端；保存期间允许继续编辑，成功后以服务器返回值建立新基线。
 */

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { RotateCcw, Save } from "lucide-react"
import { useState } from "react"
import { Link } from "react-router-dom"
import { toast } from "sonner"

import { referenceApi, referenceJsonBody } from "@/api/reference-client"
import type { ProjectReferenceStatus, ReferenceApplyPolicy } from "@/api/reference-types"
import { projectReferencesQuery, projectReferencesStatusQuery, referenceSourcesQuery } from "@/api/queries"
import { ErrorState, LoadingState } from "@/components/page-state"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Spinner } from "@/components/ui/spinner"

import { ReferenceApplicationPanel } from "./reference-application-panel"
import { ReferenceBindingsEditor } from "./reference-bindings-editor"
import { referenceDraftStore } from "./reference-draft-store"
import { defaultReferenceLimits, sameBindings, validateBindings } from "./reference-form"
import { invalidateProjectReferences, saveProjectReferences } from "./project-references-save"
import { useReferenceDraft } from "./use-reference-draft"

const STATUS_PAGE_SIZE = 20

export function ProjectReferencesPanel({ projectId }: { projectId: string }) {
  const queryClient = useQueryClient()
  const [offset, setOffset] = useState(0)

  const references = useQuery(projectReferencesQuery(projectId))
  const status = useQuery(projectReferencesStatusQuery(projectId, offset, STATUS_PAGE_SIZE))
  const sources = useQuery(referenceSourcesQuery())

  // `GET /projects/{id}/references` 只返回 durable 配置；应用事实走 status 查询。
  const configuration = references.data
  const draft = useReferenceDraft(projectId, configuration)
  const limits = defaultReferenceLimits
  const validation = validateBindings(draft?.draft ?? [], limits)

  const apply = useMutation({
    mutationFn: async (policy: ReferenceApplyPolicy) => {
      const current = referenceDraftStore.peek(projectId)
      const expectedRevision = current?.baseRevision ?? configuration?.revision ?? 0
      // 202 返回项目级状态；这里不把它写进分页缓存，统一用失效重新读取事实。
      await referenceApi<ProjectReferenceStatus>(`/projects/${projectId}/references/apply`, {
        method: "POST",
        ...referenceJsonBody({ expected_revision: expectedRevision, policy }),
      })
      return policy
    },
    onSuccess: async (policy) => {
      await invalidateProjectReferences(queryClient, projectId)
      toast.success(policy === "stop_and_apply" ? "Stopping current tasks, then applying." : "Apply requested.")
    },
    onError: (error) => toast.error(error instanceof Error ? error.message : String(error)),
  })

  if (references.isLoading || sources.isLoading) return <LoadingState rows={6} />
  if (references.error) return <ErrorState error={references.error} retry={() => void references.refetch()} />
  if (!draft || !configuration) return <LoadingState rows={3} />

  const dirty = !sameBindings(draft.original, draft.draft)
  const canSave = dirty && validation.valid && !draft.saving

  return (
    <div className="min-h-0 flex-1 overflow-auto p-5 md:p-7">
      <div className="mx-auto flex max-w-4xl flex-col gap-5">
        {draft.conflict && (
          <Alert variant="destructive">
            <AlertTitle>Configuration revision conflict</AlertTitle>
            <AlertDescription>
              <p>
                {draft.conflict.message} You tried to save on top of revision {draft.conflict.expectedRevision}, but the
                server now has revision {configuration.revision}. Nothing was overwritten.
              </p>
              <div className="mt-3 flex flex-wrap gap-2">
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  onClick={() => {
                    referenceDraftStore.resetToServer(projectId, configuration)
                    toast.success("Reloaded the server configuration and discarded local edits.")
                  }}
                >
                  Reload server revision {configuration.revision}
                </Button>
                <Button type="button" size="sm" variant="ghost" onClick={() => referenceDraftStore.dismissConflict(projectId)}>
                  Keep my draft
                </Button>
              </div>
            </AlertDescription>
          </Alert>
        )}

        <section className="flex flex-col gap-4 rounded-lg border p-5">
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div>
              <h2 className="text-sm font-semibold">Reference sources</h2>
              <p className="mt-1 text-xs text-muted-foreground" data-testid="reference-configuration-summary">
                Saved configuration revision {configuration.revision}. Sources mount read-only at
                /references/&lt;alias&gt; for every agent of this project.
              </p>
            </div>
            {dirty && (
              <span className="text-xs text-muted-foreground" data-testid="reference-draft-dirty">
                Unsaved changes
              </span>
            )}
          </div>

          <ReferenceBindingsEditor
            bindings={draft.draft}
            sources={sources.data ?? []}
            limits={limits}
            validation={validation}
            emptyAction={
              <Button asChild variant="outline" size="sm">
                <Link to="/settings/reference-sources">Register a source</Link>
              </Button>
            }
            onChange={(next) => referenceDraftStore.setBindings(projectId, next)}
          />

          <div className="flex flex-wrap items-center justify-end gap-2">
            {draft.error && !draft.conflict && <p className="mr-auto text-xs text-destructive">{draft.error}</p>}
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={!dirty || draft.saving}
              onClick={() => {
                referenceDraftStore.resetToServer(projectId, configuration)
                toast.success("Discarded local reference edits.")
              }}
            >
              <RotateCcw data-icon="inline-start" /> Reset
            </Button>
            <Button
              type="button"
              size="sm"
              disabled={!canSave}
              // 保存生命周期由草稿存储拥有：面板卸载不会中断请求，也不会卡住 saving 状态。
              onClick={() => void saveProjectReferences(projectId, queryClient)}
            >
              {draft.saving ? <Spinner data-icon="inline-start" /> : <Save data-icon="inline-start" />}
              {draft.saving ? "Saving…" : "Save and apply"}
            </Button>
          </div>
        </section>

        {status.data ? (
          <ReferenceApplicationPanel
            status={status.data}
            onPageChange={setOffset}
            onRetry={() => apply.mutate("when_idle")}
            onStopAndApply={() => apply.mutate("stop_and_apply")}
            retryPending={apply.isPending && apply.variables === "when_idle"}
            stopAndApplyPending={apply.isPending && apply.variables === "stop_and_apply"}
          />
        ) : status.isLoading ? (
          <LoadingState rows={2} />
        ) : status.error ? (
          <ErrorState error={status.error} retry={() => void status.refetch()} />
        ) : null}
      </div>
    </div>
  )
}
