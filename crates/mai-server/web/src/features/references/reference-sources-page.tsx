/**
 * 全局设置 · 参考源码。
 *
 * 管理面能力：登记固定版本目录、跟踪异步只读验证进度、显示版本来源与摘要、重新验证、
 * 修改名称/版本说明、全局禁用，以及在没有项目绑定时注销元数据（绝不删除宿主目录）。
 */

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { Plus, RefreshCw, Trash2 } from "lucide-react"
import { useState } from "react"
import { toast } from "sonner"

import { referenceApi, referenceJsonBody, ReferenceApiError } from "@/api/reference-client"
import type { PatchReferenceSourceRequest, ReferenceSource } from "@/api/reference-types"
import { queryKeys, referenceSourcePolicyQuery, referenceSourcesQuery } from "@/api/queries"
import { EmptyState, ErrorState, LoadingState } from "@/components/page-state"
import { StatusBadge } from "@/components/status"
import { Alert, AlertDescription } from "@/components/ui/alert"
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Spinner } from "@/components/ui/spinner"
import { Switch } from "@/components/ui/switch"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { SettingsBody, SettingsHeader } from "@/features/settings/settings-section"

import { referenceFieldMessage, RegisterReferenceSourceDialog } from "./register-reference-source-dialog"

export function ReferenceSourcesPage() {
  const queryClient = useQueryClient()
  const sources = useQuery(referenceSourcesQuery())
  const policy = useQuery(referenceSourcePolicyQuery())
  const [registerOpen, setRegisterOpen] = useState(false)
  const [editing, setEditing] = useState<ReferenceSource | null>(null)

  const refreshSources = async () => {
    await queryClient.invalidateQueries({ queryKey: queryKeys.referenceSources })
  }

  const verify = useMutation({
    // 202 返回该源的 validating 记录。
    mutationFn: (id: string) => referenceApi<ReferenceSource>(`/reference-sources/${id}/verify`, { method: "POST" }),
    onSuccess: async () => {
      toast.success("Re-validation queued.")
      await refreshSources()
    },
    onError: (error) => toast.error(error instanceof Error ? error.message : String(error)),
  })

  const patch = useMutation({
    mutationFn: ({ id, body }: { id: string; body: PatchReferenceSourceRequest }) =>
      referenceApi<ReferenceSource>(`/reference-sources/${id}`, { method: "PATCH", ...referenceJsonBody(body) }),
    onSuccess: async () => {
      toast.success("Source metadata saved.")
      setEditing(null)
      await refreshSources()
    },
    onError: (error) => toast.error(referenceErrorMessage(error)),
  })

  const remove = useMutation({
    // 204 No Content；注销只删元数据，绝不删宿主目录。
    mutationFn: (id: string) => referenceApi<null>(`/reference-sources/${id}`, { method: "DELETE" }),
    onSuccess: async () => {
      toast.success("Source unregistered. The host directory itself was not touched.")
      await refreshSources()
    },
    onError: (error) => toast.error(referenceErrorMessage(error)),
  })

  if (sources.isLoading) return <LoadingState rows={6} />
  if (sources.error) return <ErrorState error={sources.error} retry={() => void sources.refetch()} />

  const items = sources.data ?? []
  const validating = items.filter((source) => source.state === "validating").length

  return (
    <>
      <SettingsHeader
        title="Reference Sources"
        description="Register fixed read-only source snapshots from the Docker host and track verification."
        status={<StatusBadge status={validating > 0 ? "validating" : "ready"} />}
      />
      <SettingsBody>
        <div className="flex flex-col gap-5">
          <p className="text-xs text-muted-foreground">
            Registering a directory does not grant it to every agent. Projects choose registered sources explicitly, and
            the host directory is never deleted from this page.
            {validating > 0 ? ` ${validating} registration(s) are still validating.` : ""}
          </p>

          <div className="flex justify-end">
            <Button type="button" onClick={() => setRegisterOpen(true)}>
              <Plus data-icon="inline-start" /> Register source
            </Button>
          </div>

          {items.length === 0 ? (
            <EmptyState
              title="No reference sources yet"
              description="Register a published snapshot directory on the Docker host to make it selectable by projects."
            />
          ) : (
            <div className="overflow-hidden rounded-lg border">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Source</TableHead>
                    <TableHead>State</TableHead>
                    <TableHead>Snapshot</TableHead>
                    <TableHead className="text-right">Actions</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {items.map((source) => (
                    <TableRow key={source.id}>
                      <TableCell className="align-top">
                        <div className="flex flex-col gap-1">
                          <span className="text-sm font-medium">{source.name}</span>
                          <span className="break-all font-mono text-[11px] text-muted-foreground">{source.host_path}</span>
                          <span className="text-[11px] text-muted-foreground">
                            {source.revision_label}
                            {source.approved_for_agent_access ? " · approved for agent access" : " · not approved"}
                          </span>
                        </div>
                      </TableCell>
                      <TableCell className="align-top">
                        <div className="flex flex-col gap-1">
                          <StatusBadge status={source.state} />
                          {source.state === "validating" && (
                            <span className="flex items-center gap-1 text-[11px] text-muted-foreground">
                              <Spinner className="size-3" /> scanning
                            </span>
                          )}
                          {source.validation_error && (
                            <span className="text-[11px] text-destructive">{source.validation_error.message}</span>
                          )}
                        </div>
                      </TableCell>
                      <TableCell className="align-top text-xs text-muted-foreground">
                        <div className="flex flex-col gap-1">
                          <span>{source.provenance.kind === "git_tree" ? "Git snapshot" : "Directory snapshot"}</span>
                          {source.provenance.commit_sha && (
                            <span className="font-mono text-[11px]">{source.provenance.commit_sha}</span>
                          )}
                          <span>
                            {source.file_count} files · {source.byte_count} bytes
                          </span>
                          {source.manifest_digest && (
                            <span className="font-mono text-[11px]">digest {source.manifest_digest}</span>
                          )}
                        </div>
                      </TableCell>
                      <TableCell className="align-top">
                        <div className="flex flex-wrap justify-end gap-2">
                          <Button
                            type="button"
                            variant="outline"
                            size="sm"
                            disabled={verify.isPending && verify.variables === source.id}
                            onClick={() => verify.mutate(source.id)}
                          >
                            <RefreshCw data-icon="inline-start" /> Verify
                          </Button>
                          <Button type="button" variant="outline" size="sm" onClick={() => setEditing(source)}>
                            Edit
                          </Button>
                          <DisableToggle source={source} onToggle={(disabled) => patch.mutate({ id: source.id, body: { disabled } })} pending={patch.isPending} />
                          <AlertDialog>
                            <AlertDialogTrigger asChild>
                              <Button type="button" variant="destructive" size="sm" disabled={remove.isPending}>
                                <Trash2 data-icon="inline-start" /> Unregister
                              </Button>
                            </AlertDialogTrigger>
                            <AlertDialogContent>
                              <AlertDialogHeader>
                                <AlertDialogTitle>Unregister {source.name}?</AlertDialogTitle>
                                <AlertDialogDescription>
                                  This only removes the registration metadata. The host directory is not deleted.
                                  Unregistering is rejected while any project config, applied binding or in-flight
                                  operation still references this source.
                                </AlertDialogDescription>
                              </AlertDialogHeader>
                              <AlertDialogFooter>
                                <AlertDialogCancel>Cancel</AlertDialogCancel>
                                <AlertDialogAction
                                  variant="destructive"
                                  aria-label={`Confirm unregister ${source.name}`}
                                  onClick={() => remove.mutate(source.id)}
                                >
                                  Unregister
                                </AlertDialogAction>
                              </AlertDialogFooter>
                            </AlertDialogContent>
                          </AlertDialog>
                        </div>
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </div>
          )}

          <section className="rounded-lg border p-5">
            <h2 className="text-sm font-semibold">Deployment policy</h2>
            <p className="mt-1 text-xs text-muted-foreground">Read-only from the server configuration; the UI cannot widen these limits.</p>
            {policy.isLoading ? (
              <LoadingState rows={2} />
            ) : policy.data ? (
              <div className="mt-3 grid gap-2 text-xs text-muted-foreground sm:grid-cols-2">
                <span>Allowed roots: <span className="font-mono">{policy.data.allowed_roots.join(", ") || "—"}</span></span>
                <span>Container root: <span className="font-mono">{policy.data.reference_container_root}</span></span>
                <span>Alias pattern: <span className="font-mono">{policy.data.alias_pattern}</span></span>
                <span>Max bindings per project: {policy.data.max_bindings_per_project}</span>
                <span>Provenance kinds: {policy.data.supported_provenance_kinds.join(", ")}</span>
                <span>Minimum Docker: {policy.data.minimum_docker_engine_version} · kernel {policy.data.minimum_kernel_version}</span>
              </div>
            ) : (
              <Alert variant="destructive" className="mt-3">
                <AlertDescription>{policy.error instanceof Error ? policy.error.message : "Policy unavailable."}</AlertDescription>
              </Alert>
            )}
            {policy.data && !policy.data.require_strict_recursive_read_only && (
              <Alert variant="destructive" className="mt-3">
                <AlertDescription>
                  Strict recursive read-only mounts have not been verified on this host, so reference sources cannot be
                  applied to agent environments yet.
                </AlertDescription>
              </Alert>
            )}
          </section>
        </div>
      </SettingsBody>

      <RegisterReferenceSourceDialog open={registerOpen} onOpenChange={setRegisterOpen} />
      {editing && (
        <EditReferenceSourceDialog
          source={editing}
          pending={patch.isPending}
          onOpenChange={(open) => !open && setEditing(null)}
          onSave={(body) => patch.mutate({ id: editing.id, body })}
        />
      )}
    </>
  )
}

function DisableToggle({ source, onToggle, pending }: { source: ReferenceSource; onToggle(disabled: boolean): void; pending: boolean }) {
  const disabled = source.state === "disabled"
  return (
    <div className="flex items-center gap-2 rounded-md border px-2 py-1">
      <Switch
        id={`reference-source-disabled-${source.id}`}
        checked={disabled}
        disabled={pending}
        aria-label={`Disable ${source.name}`}
        onCheckedChange={(checked: boolean) => onToggle(checked)}
      />
      <FieldLabel htmlFor={`reference-source-disabled-${source.id}`} className="text-xs font-normal">
        Disabled
      </FieldLabel>
    </div>
  )
}

function EditReferenceSourceDialog({
  source,
  pending,
  onOpenChange,
  onSave,
}: {
  source: ReferenceSource
  pending: boolean
  onOpenChange(open: boolean): void
  onSave(body: PatchReferenceSourceRequest): void
}) {
  const [name, setName] = useState(source.name)
  const [revisionLabel, setRevisionLabel] = useState(source.revision_label)
  const [approved, setApproved] = useState(source.approved_for_agent_access)

  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>Edit source metadata</DialogTitle>
          <DialogDescription>
            Path, provenance and digest form the registered identity and cannot be changed here. Changing the content of
            a directory requires a new registration.
          </DialogDescription>
        </DialogHeader>
        <FieldGroup>
          <Field>
            <FieldLabel htmlFor="edit-reference-source-name">Name</FieldLabel>
            <Input id="edit-reference-source-name" value={name} onChange={(event) => setName(event.target.value)} />
          </Field>
          <Field>
            <FieldLabel htmlFor="edit-reference-source-revision">Version label</FieldLabel>
            <Input id="edit-reference-source-revision" value={revisionLabel} onChange={(event) => setRevisionLabel(event.target.value)} />
          </Field>
          <Field orientation="horizontal">
            <Switch id="edit-reference-source-approved" checked={approved} onCheckedChange={setApproved} />
            <FieldLabel htmlFor="edit-reference-source-approved" className="text-sm font-normal">
              Confirmed suitable for agent and model access
            </FieldLabel>
          </Field>
          <FieldDescription className="font-mono">{source.host_path}</FieldDescription>
        </FieldGroup>
        <DialogFooter>
          <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button
            type="button"
            disabled={pending}
            onClick={() => onSave({
              name: name.trim() || source.name,
              revision_label: revisionLabel.trim() || source.revision_label,
              approved_for_agent_access: approved,
            })}
          >
            {pending && <Spinner data-icon="inline-start" />} Save metadata
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function referenceErrorMessage(error: unknown): string {
  if (error instanceof ReferenceApiError) {
    // 状态类操作（禁用、注销）优先展示服务端人类消息；字段级细节只在没有消息时补位。
    return error.response?.message || referenceFieldMessage(error.response, error.message)
  }
  return error instanceof Error ? error.message : String(error)
}
