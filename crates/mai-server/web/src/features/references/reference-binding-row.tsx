/**
 * 单个项目参考源绑定的编辑行。
 *
 * 受控组件：所有状态由父级草稿提供。即使所选源当前不可用（disabled/missing/invalid）
 * 也要保持可编辑、可禁用、可删除，因为"目录不存在"不能成为修复坏配置的障碍。
 */

import { Trash2 } from "lucide-react"

import type { ProjectReferenceBinding, ReferenceSource } from "@/api/reference-types"
import { StatusBadge } from "@/components/status"
import { Button } from "@/components/ui/button"
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import { Textarea } from "@/components/ui/textarea"

import { aliasContainerPath, type BindingRowErrors } from "./reference-form"

/** 选择器里代表"未知/已注销源"的哨兵值，避免与真实 UUID 冲突。 */
const UNKNOWN_SOURCE = "__mai_unknown_source__"

export function ReferenceBindingRow({
  binding,
  sources,
  errors,
  disabled,
  maxDescriptionChars,
  onChange,
  onRemove,
}: {
  binding: ProjectReferenceBinding
  sources: ReferenceSource[]
  errors?: BindingRowErrors
  disabled?: boolean
  maxDescriptionChars: number
  onChange(next: ProjectReferenceBinding): void
  onRemove(): void
}) {
  const known = sources.find((source) => source.id === binding.source_id)
  const selectable = sources.filter((source) => source.state === "ready")
  // 即使当前源不是 ready，也要让它的 name 出现在下拉里，用户才能识别并改选。
  const options = known && !selectable.some((source) => source.id === known.id) ? [known, ...selectable] : selectable
  const aliasId = `reference-alias-${binding.id}`
  const descriptionId = `reference-description-${binding.id}`

  return (
    <div className="min-w-0 rounded-lg border p-4" data-reference-binding={binding.id}>
      <FieldGroup>
        <div className="grid min-w-0 grid-cols-1 gap-4 md:grid-cols-2">
          <Field className="min-w-0" data-invalid={errors?.source ? "true" : undefined}>
            <FieldLabel htmlFor={`reference-source-${binding.id}`}>Source</FieldLabel>
            <Select
              value={known ? binding.source_id : UNKNOWN_SOURCE}
              onValueChange={(value: string) => onChange({ ...binding, source_id: value === UNKNOWN_SOURCE ? "" : value })}
              disabled={disabled}
            >
              <SelectTrigger id={`reference-source-${binding.id}`} className="w-full min-w-0">
                <SelectValue className="min-w-0 flex-1 overflow-hidden" placeholder="Select a registered source" />
              </SelectTrigger>
              <SelectContent>
                <SelectGroup>
                  {options.map((source) => (
                    <SelectItem key={source.id} value={source.id}>
                      <span className="flex min-w-0 items-center gap-2">
                        <span className="truncate">{source.name}</span>
                        {source.state !== "ready" && <StatusBadge status={source.state} />}
                      </span>
                    </SelectItem>
                  ))}
                  {!known && <SelectItem value={UNKNOWN_SOURCE}>Unknown source</SelectItem>}
                </SelectGroup>
              </SelectContent>
            </Select>
            <FieldError>{errors?.source}</FieldError>
          </Field>

          <Field className="min-w-0" data-invalid={errors?.alias ? "true" : undefined}>
            <FieldLabel htmlFor={aliasId}>Alias</FieldLabel>
            <Input
              id={aliasId}
              value={binding.alias}
              disabled={disabled}
              spellCheck={false}
              aria-invalid={errors?.alias ? true : undefined}
              onChange={(event) => onChange({ ...binding, alias: event.target.value })}
            />
            <FieldDescription className="font-mono">{aliasContainerPath(binding.alias)}</FieldDescription>
            <FieldError>{errors?.alias}</FieldError>
          </Field>
        </div>

        <Field className="min-w-0" data-invalid={errors?.description ? "true" : undefined}>
          <FieldLabel htmlFor={descriptionId}>Purpose</FieldLabel>
          <Textarea
            id={descriptionId}
            className="min-h-20"
            value={binding.description}
            disabled={disabled}
            maxLength={maxDescriptionChars}
            placeholder="Read-only reference for driver and kernel design."
            onChange={(event) => onChange({ ...binding, description: event.target.value })}
          />
          <FieldError>{errors?.description}</FieldError>
        </Field>

        <div className="flex items-center justify-between gap-3">
          <Field orientation="horizontal" className="w-auto">
            <Switch
              id={`reference-enabled-${binding.id}`}
              checked={binding.enabled}
              disabled={disabled}
              onCheckedChange={(checked: boolean) => onChange({ ...binding, enabled: checked })}
            />
            <FieldLabel htmlFor={`reference-enabled-${binding.id}`} className="text-xs font-normal">
              Mount for agents
            </FieldLabel>
          </Field>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            disabled={disabled}
            aria-label={`Remove binding ${binding.alias || binding.id}`}
            onClick={onRemove}
          >
            <Trash2 data-icon="inline-start" /> Remove
          </Button>
        </div>
      </FieldGroup>
    </div>
  )
}
