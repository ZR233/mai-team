/**
 * 项目参考源绑定编辑器。
 *
 * 受控组件：草稿、校验结果与源清单都由父级提供，因此设置页对话框与项目面板可以复用
 * 同一份编辑逻辑（同一个 alias 规则、同一个 `/references/<alias>` 预览），不会出现
 * 第二套路径校验。
 */

import { Plus } from "lucide-react"
import type { ReactNode } from "react"

import type { ProjectReferenceBinding, ReferenceSource } from "@/api/reference-types"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"

import { newBindingId, type BindingsValidation, type ReferenceLimits } from "./reference-form"
import { ReferenceBindingRow } from "./reference-binding-row"

export function ReferenceBindingsEditor({
  bindings,
  sources,
  limits,
  validation,
  disabled,
  emptyAction,
  onChange,
}: {
  bindings: ProjectReferenceBinding[]
  sources: ReferenceSource[]
  limits: ReferenceLimits
  validation: BindingsValidation
  disabled?: boolean
  /** 尚未登记任何 ready 源时展示的补充操作（例如"登记新源"）。 */
  emptyAction?: ReactNode
  onChange(next: ProjectReferenceBinding[]): void
}) {
  const readySources = sources.filter((source) => source.state === "ready")
  const atLimit = bindings.length >= limits.maxBindings

  const addBinding = () => {
    const next: ProjectReferenceBinding = {
      id: newBindingId(),
      source_id: readySources[0]?.id ?? "",
      alias: "",
      description: "",
      enabled: true,
    }
    onChange([...bindings, next])
  }

  return (
    <div className="flex flex-col gap-4">
      {validation.general.map((message) => (
        <Alert key={message} variant="destructive">
          <AlertDescription>{message}</AlertDescription>
        </Alert>
      ))}

      {bindings.length === 0 ? (
        <div className="rounded-lg border border-dashed p-5 text-center">
          <p className="text-sm font-medium">No reference sources bound</p>
          <p className="mt-1 text-xs text-muted-foreground">
            {readySources.length === 0
              ? "Register and verify a source before binding it to this project."
              : "Bind a registered source to mount it read-only at /references/<alias>."}
          </p>
          <div className="mt-4 flex flex-wrap items-center justify-center gap-2">
            <Button type="button" variant="outline" onClick={addBinding} disabled={disabled || readySources.length === 0}>
              <Plus data-icon="inline-start" /> Add binding
            </Button>
            {readySources.length === 0 && emptyAction}
          </div>
        </div>
      ) : (
        <div className="flex flex-col gap-3">
          {bindings.map((binding) => (
            <ReferenceBindingRow
              key={binding.id}
              binding={binding}
              sources={sources}
              errors={validation.byId[binding.id]}
              disabled={disabled}
              maxDescriptionChars={limits.maxDescriptionChars}
              onChange={(next) => onChange(bindings.map((candidate) => (candidate.id === next.id ? next : candidate)))}
              onRemove={() => onChange(bindings.filter((candidate) => candidate.id !== binding.id))}
            />
          ))}
          <div className="flex items-center justify-between gap-3">
            <Button type="button" variant="outline" size="sm" onClick={addBinding} disabled={disabled || atLimit}>
              <Plus data-icon="inline-start" /> Add binding
            </Button>
            <p className="text-xs text-muted-foreground">
              {bindings.length} / {limits.maxBindings}
            </p>
          </div>
        </div>
      )}
    </div>
  )
}
