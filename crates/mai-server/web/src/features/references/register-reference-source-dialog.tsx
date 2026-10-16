/**
 * 登记参考源对话框。
 *
 * 登记的是"运行 Docker 的服务器目录"，不是浏览器所在电脑的目录；请求体只包含协议定义的
 * 四个字段（name/host_path/revision_label/approved_for_agent_access）。版本来源与摘要由
 * 服务端验证器读取发布 manifest 后写入，前端不声明任何 commit。
 */

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { useEffect, useState } from "react"
import { toast } from "sonner"

import { referenceApi, referenceJsonBody, ReferenceApiError } from "@/api/reference-client"
import type { NewReferenceSourceRequest, ReferenceErrorResponse, ReferenceSource } from "@/api/reference-types"
import { queryKeys, referenceSourcePolicyQuery } from "@/api/queries"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Spinner } from "@/components/ui/spinner"
import { Switch } from "@/components/ui/switch"

import { defaultReferenceLimits, sourceDraftIsValid, validateSourceDraft, type ReferenceSourceDraft } from "./reference-form"

const EMPTY_DRAFT: ReferenceSourceDraft = {
  name: "",
  host_path: "",
  revision_label: "",
  approved_for_agent_access: false,
}

export function RegisterReferenceSourceDialog({
  open,
  onOpenChange,
  onRegistered,
}: {
  open: boolean
  onOpenChange(open: boolean): void
  /** 202 返回的新登记项（状态为 validating）。 */
  onRegistered?(source: ReferenceSource): void
}) {
  const queryClient = useQueryClient()
  const policy = useQuery({ ...referenceSourcePolicyQuery(), enabled: open })
  const [draft, setDraft] = useState<ReferenceSourceDraft>(EMPTY_DRAFT)
  const [touched, setTouched] = useState(false)

  useEffect(() => {
    if (!open) {
      setDraft(EMPTY_DRAFT)
      setTouched(false)
    }
  }, [open])

  const limits = policy.data
    ? { ...defaultReferenceLimits, maxSourceNameChars: policy.data.source_name_max_chars, maxRevisionLabelChars: policy.data.revision_label_max_chars }
    : defaultReferenceLimits
  const errors = validateSourceDraft(draft, limits)
  const valid = sourceDraftIsValid(errors)

  const register = useMutation({
    // 202 返回刚建立的 validating 记录。
    mutationFn: (request: NewReferenceSourceRequest) => referenceApi<ReferenceSource>("/reference-sources", {
      method: "POST",
      ...referenceJsonBody(request),
    }),
    onSuccess: async (source) => {
      await queryClient.invalidateQueries({ queryKey: queryKeys.referenceSources })
      toast.success("Registration accepted. Read-only validation is running.")
      onOpenChange(false)
      onRegistered?.(source)
    },
    onError: (error) => {
      const message = error instanceof ReferenceApiError
        ? referenceFieldMessage(error.response, error.message)
        : error.message
      toast.error(message)
    },
  })

  const submit = () => {
    setTouched(true)
    if (!valid) return
    register.mutate({
      name: draft.name.trim(),
      host_path: draft.host_path.trim(),
      revision_label: draft.revision_label.trim(),
      approved_for_agent_access: draft.approved_for_agent_access,
    })
  }

  const showError = (key: keyof typeof errors) => (touched ? errors[key] : undefined)

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[92vh] overflow-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>Register reference source</DialogTitle>
          <DialogDescription>
            Register a fixed, read-only snapshot directory that lives on the Docker host. The directory itself is never
            copied into agent images or mounted into every project automatically.
          </DialogDescription>
        </DialogHeader>

        <FieldGroup>
          <Field data-invalid={showError("name") ? "true" : undefined}>
            <FieldLabel htmlFor="reference-source-name">Name</FieldLabel>
            <Input
              id="reference-source-name"
              value={draft.name}
              aria-invalid={showError("name") ? true : undefined}
              onChange={(event) => setDraft({ ...draft, name: event.target.value })}
              placeholder="Linux driver reference"
            />
            <FieldError>{showError("name")}</FieldError>
          </Field>

          <Field data-invalid={showError("host_path") ? "true" : undefined}>
            <FieldLabel htmlFor="reference-source-path">Host path (server)</FieldLabel>
            <Input
              id="reference-source-path"
              value={draft.host_path}
              spellCheck={false}
              className="font-mono"
              aria-invalid={showError("host_path") ? true : undefined}
              onChange={(event) => setDraft({ ...draft, host_path: event.target.value })}
              placeholder="/srv/mai-references/linux/<commit>/repo"
            />
            <FieldDescription>
              Absolute path on the machine running Docker, not on the computer showing this page.
              {policy.data?.allowed_roots.length
                ? ` Allowed roots: ${policy.data.allowed_roots.join(", ")}.`
                : ""}
            </FieldDescription>
            <FieldError>{showError("host_path")}</FieldError>
          </Field>

          <Field data-invalid={showError("revision_label") ? "true" : undefined}>
            <FieldLabel htmlFor="reference-source-revision">Version label</FieldLabel>
            <Input
              id="reference-source-revision"
              value={draft.revision_label}
              aria-invalid={showError("revision_label") ? true : undefined}
              onChange={(event) => setDraft({ ...draft, revision_label: event.target.value })}
              placeholder="Published Linux v6.12 snapshot"
            />
            <FieldError>{showError("revision_label")}</FieldError>
          </Field>

          <Field orientation="horizontal" className="rounded-lg border p-3" data-invalid={showError("approved_for_agent_access") ? "true" : undefined}>
            <Switch
              id="reference-source-approved"
              checked={draft.approved_for_agent_access}
              onCheckedChange={(checked: boolean) => setDraft({ ...draft, approved_for_agent_access: checked })}
            />
            <div className="flex flex-col gap-1">
              <FieldLabel htmlFor="reference-source-approved">
                I confirm this directory is suitable to expose to agents and models
              </FieldLabel>
              <FieldDescription>
                Path checks cannot prove the absence of secrets. Private keys, credentials and licence-restricted
                material must be removed before registering.
              </FieldDescription>
              <FieldError>{showError("approved_for_agent_access")}</FieldError>
            </div>
          </Field>

          {policy.data && !policy.data.require_strict_recursive_read_only && (
            <Alert variant="destructive">
              <AlertDescription>
                This deployment has not verified strict recursive read-only support yet, so agents cannot be given
                write-protected reference mounts.
              </AlertDescription>
            </Alert>
          )}
        </FieldGroup>

        <DialogFooter>
          <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="button" disabled={register.isPending} onClick={submit}>
            {register.isPending && <Spinner data-icon="inline-start" />}
            {register.isPending ? "Registering…" : "Register and verify"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

/** 把字段级错误优先展示为人类消息。 */
export function referenceFieldMessage(response: ReferenceErrorResponse | null, fallback: string): string {
  const fieldMessage = response?.field_errors[0]?.message
  return fieldMessage || response?.message || fallback
}
