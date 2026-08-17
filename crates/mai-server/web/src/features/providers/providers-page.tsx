import { useQuery, useQueryClient } from "@tanstack/react-query"
import { Check, LoaderCircle, Pencil, Plus, TestTube2, Trash2 } from "lucide-react"
import { useMemo, useState } from "react"
import { toast } from "sonner"

import { api, jsonBody } from "@/api/client"
import type { PlProviderConfig, ProviderCatalog, ProviderInstance, ProvidersResponse, ProviderTestResponse } from "@/api/product-types"
import { providerCatalogQuery, providersQuery, queryKeys } from "@/api/queries"
import { EmptyState, ErrorState, LoadingState } from "@/components/page-state"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle, AlertDialogTrigger } from "@/components/ui/alert-dialog"
import { Avatar, AvatarFallback } from "@/components/ui/avatar"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Sheet, SheetContent, SheetDescription, SheetFooter, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { Spinner } from "@/components/ui/spinner"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { Textarea } from "@/components/ui/textarea"
import { WorkspaceHeader } from "@/components/workspace-header"

type ProviderRequestPayload =
  | {
      id: string
      source: "preset"
      preset_id: string
      name: string
      base_url: string
      bearer_token?: string
      bearer_token_env?: string | null
      http_headers?: Record<string, string>
    }
  | { id: string; source: "custom"; config: PlProviderConfig }

export default function ProvidersPage() {
  const queryClient = useQueryClient()
  const catalog = useQuery(providerCatalogQuery())
  const providers = useQuery(providersQuery())
  const [editing, setEditing] = useState<number | "new" | null>(null)
  const [testingId, setTestingId] = useState<string | null>(null)

  if (catalog.isLoading || providers.isLoading) return <ProvidersFrame><LoadingState rows={6} /></ProvidersFrame>
  if (catalog.error) return <ProvidersFrame><ErrorState error={catalog.error} retry={() => void catalog.refetch()} /></ProvidersFrame>
  if (providers.error) return <ProvidersFrame><ErrorState error={providers.error} retry={() => void providers.refetch()} /></ProvidersFrame>
  if (!catalog.data || !providers.data) return null

  const saveAll = async (nextProviders: ProviderRequestPayload[]) => {
    await api<ProvidersResponse>("/providers", { method: "PUT", ...jsonBody({ providers: nextProviders }) })
    await queryClient.invalidateQueries({ queryKey: queryKeys.providers })
  }
  const remove = async (index: number) => {
    const next = providers.data.providers.filter((_, candidate) => candidate !== index).map(providerRequest)
    try {
      await saveAll(next)
      toast.success("Provider removed")
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error))
    }
  }
  const testProvider = async (provider: ProviderInstance) => {
    setTestingId(provider.id)
    try {
      const result = await api<ProviderTestResponse>(`/providers/${encodeURIComponent(provider.id)}/test`, {
        method: "POST",
        ...jsonBody({ model: provider.models[0]?.slug ?? null, reasoning_effort: null, deep: true }),
      })
      if (!result.ok) throw new Error(result.error || "Provider test failed")
      toast.success(`${provider.config.name} connected in ${result.latency_ms} ms`, {
        description: result.output_preview || `${result.transport.protocol} · ${result.model}`,
      })
    } catch (error) {
      toast.error(`${provider.config.name} connection failed`, {
        description: error instanceof Error ? error.message : String(error),
      })
    } finally {
      setTestingId(null)
    }
  }

  return <div className="flex h-full min-h-0 flex-col bg-background">
    <WorkspaceHeader crumbs={[{ label: "Providers" }]} actions={<Button onClick={() => setEditing("new")}><Plus data-icon="inline-start" /> Add provider</Button>} />
    <div className="min-h-0 flex-1 overflow-auto"><div className="mx-auto flex max-w-6xl flex-col gap-5 p-4 md:p-6">
      <div><h2 className="text-base font-semibold">Provider instances</h2><p className="text-sm text-muted-foreground">Preset model, transport, capability, and request semantics come directly from PL.</p></div>
      {providers.data.providers.length === 0
        ? <EmptyState title="No providers configured" description="Create an instance from the PL catalog or provide a complete custom PL configuration." action={<Button onClick={() => setEditing("new")}><Plus data-icon="inline-start" /> Add provider</Button>} />
        : <div className="overflow-hidden rounded-lg border"><Table><TableHeader><TableRow><TableHead>Provider</TableHead><TableHead className="hidden md:table-cell">Transport</TableHead><TableHead className="hidden lg:table-cell">Models</TableHead><TableHead className="w-32 text-right">Actions</TableHead></TableRow></TableHeader><TableBody>{providers.data.providers.map((provider, index) => {
          const primaryModel = provider.models[0]
          return <TableRow key={provider.id}>
            <TableCell><div className="flex items-center gap-3"><Avatar className="size-9 rounded-lg"><AvatarFallback className="rounded-lg">{provider.config.name.slice(0, 1).toUpperCase()}</AvatarFallback></Avatar><div className="min-w-0"><span className="font-medium">{provider.config.name}</span><div className="max-w-72 truncate text-xs text-muted-foreground">{provider.id} · {provider.config.base_url}</div></div></div></TableCell>
            <TableCell className="hidden md:table-cell"><span className="text-sm">{primaryModel?.transport.protocol ?? "—"}</span><span className="block text-xs text-muted-foreground">{primaryModel?.transport.default_connection_mode ?? "—"}</span></TableCell>
            <TableCell className="hidden lg:table-cell"><span className="font-medium">{primaryModel?.slug ?? "—"}</span>{provider.models.length > 1 && <span className="ml-1 text-xs text-muted-foreground">+{provider.models.length - 1}</span>}</TableCell>
            <TableCell><div className="flex justify-end gap-1"><Button variant="ghost" size="icon" disabled={testingId === provider.id || provider.models.length === 0} onClick={() => void testProvider(provider)} aria-label={`Test ${provider.config.name}`}>{testingId === provider.id ? <LoaderCircle data-icon="inline-start" className="animate-spin" /> : <TestTube2 data-icon="inline-start" />}</Button><Button variant="ghost" size="icon" onClick={() => setEditing(index)} aria-label={`Edit ${provider.config.name}`}><Pencil data-icon="inline-start" /></Button><AlertDialog><AlertDialogTrigger asChild><Button variant="ghost" size="icon" aria-label={`Delete ${provider.config.name}`}><Trash2 data-icon="inline-start" /></Button></AlertDialogTrigger><AlertDialogContent><AlertDialogHeader><AlertDialogTitle>Delete {provider.config.name}?</AlertDialogTitle><AlertDialogDescription>Roles using this provider must be reassigned before it can be removed.</AlertDialogDescription></AlertDialogHeader><AlertDialogFooter><AlertDialogCancel>Cancel</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={() => void remove(index)}>Delete provider</AlertDialogAction></AlertDialogFooter></AlertDialogContent></AlertDialog></div></TableCell>
          </TableRow>
        })}</TableBody></Table></div>}
    </div></div>
    {editing !== null && <ProviderEditor catalog={catalog.data} response={providers.data} index={editing} onClose={() => setEditing(null)} onSave={async (provider) => {
      const next = providers.data.providers.map(providerRequest)
      if (editing === "new") next.push(provider)
      else next[editing] = provider
      await saveAll(next)
      setEditing(null)
      toast.success("Provider saved")
    }} />}
  </div>
}

function ProvidersFrame({ children }: { children: React.ReactNode }) {
  return <div className="flex h-full min-h-0 flex-col"><WorkspaceHeader crumbs={[{ label: "Providers" }]} /><div className="min-h-0 flex-1 overflow-auto">{children}</div></div>
}

interface ProviderForm {
  source: "preset" | "custom"
  id: string
  presetId: string
  name: string
  baseUrl: string
  bearerToken: string
  clearBearerToken: boolean
  bearerTokenEnv: string
  headersText: string
  clearHeaders: boolean
  customConfig: string
}

function ProviderEditor({ catalog, response, index, onClose, onSave }: {
  catalog: ProviderCatalog
  response: ProvidersResponse
  index: number | "new"
  onClose(): void
  onSave(provider: ProviderRequestPayload): Promise<void>
}) {
  const existing = index === "new" ? null : response.providers[index]
  const initial = useMemo(() => formFromProvider(catalog, response, existing, index), [catalog, existing, index, response])
  const [form, setForm] = useState(initial)
  const [error, setError] = useState("")
  const [saving, setSaving] = useState(false)
  const preset = catalog.presets.find((candidate) => candidate.id === form.presetId)

  const chooseSource = (value: string) => {
    if (value === "custom") {
      setForm((current) => ({ ...current, source: "custom", presetId: "", customConfig: current.source === "custom" ? current.customConfig : customTemplate(current) }))
      return
    }
    const next = catalog.presets.find((candidate) => candidate.id === value)
    if (!next) return
    setForm((current) => ({
      ...current,
      source: "preset",
      id: uniqueProviderId(next.id, response.providers, index),
      presetId: next.id,
      name: next.display_name,
      baseUrl: next.base_url,
      bearerTokenEnv: next.credential?.env_var || "",
      bearerToken: "",
      clearBearerToken: false,
      headersText: "",
      clearHeaders: false,
    }))
  }

  const submit = async () => {
    setError("")
    if (!form.id.trim()) return setError("Provider ID is required.")
    let provider: ProviderRequestPayload
    if (form.source === "preset") {
      if (!form.presetId || !form.name.trim() || !form.baseUrl.trim()) return setError("Preset, name, and base URL are required.")
      const headers = parseHeaders(form.headersText)
      if (headers.error) return setError(headers.error)
      provider = {
        id: form.id.trim(),
        source: "preset",
        preset_id: form.presetId,
        name: form.name.trim(),
        base_url: form.baseUrl.trim(),
        ...(form.clearBearerToken ? { bearer_token: "" } : form.bearerToken ? { bearer_token: form.bearerToken } : {}),
        bearer_token_env: form.bearerTokenEnv || null,
        ...(form.clearHeaders ? { http_headers: {} } : headers.value ? { http_headers: headers.value } : {}),
      }
    } else {
      const config = parseProviderConfig(form.customConfig)
      if (config.error) return setError(config.error)
      provider = { id: form.id.trim(), source: "custom", config: config.value as PlProviderConfig }
    }
    setSaving(true)
    try {
      await onSave(provider)
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setSaving(false)
    }
  }

  return <Sheet open onOpenChange={(open: boolean) => { if (!open) onClose() }}>
    <SheetContent className="w-full gap-0 sm:max-w-2xl">
      <SheetHeader><SheetTitle>{existing ? `Edit ${existing.config.name}` : "Add provider"}</SheetTitle><SheetDescription>Preset semantics are immutable PL catalog values. Custom providers use a complete PL ProviderConfig document.</SheetDescription></SheetHeader>
      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-2"><FieldGroup className="grid sm:grid-cols-2">
        <Field><FieldLabel>Source</FieldLabel><Select value={form.source === "preset" ? form.presetId : "custom"} onValueChange={chooseSource}><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectGroup>{catalog.presets.map((item) => <SelectItem key={item.id} value={item.id}>{item.display_name}</SelectItem>)}<SelectItem value="custom">Custom PL configuration</SelectItem></SelectGroup></SelectContent></Select></Field>
        <Field><FieldLabel htmlFor="provider-id">Provider ID</FieldLabel><Input id="provider-id" value={form.id} onChange={(event) => setForm({ ...form, id: event.target.value })} /></Field>
        {form.source === "preset" ? <>
          <Field><FieldLabel htmlFor="provider-name">Display name</FieldLabel><Input id="provider-name" value={form.name} onChange={(event) => setForm({ ...form, name: event.target.value })} /></Field>
          <Field><FieldLabel htmlFor="provider-url">Base URL override</FieldLabel><Input id="provider-url" value={form.baseUrl} onChange={(event) => setForm({ ...form, baseUrl: event.target.value })} /></Field>
          <Field><FieldLabel htmlFor="provider-api-key">{preset?.credential?.label || "API key"}</FieldLabel><Input id="provider-api-key" type="password" value={form.bearerToken} disabled={form.clearBearerToken} onChange={(event) => setForm({ ...form, bearerToken: event.target.value })} placeholder={existing?.has_api_key ? "Leave blank to keep existing" : "Optional when env is set"} /><FieldDescription>Write-only; blank keeps the saved token.</FieldDescription></Field>
          <Field><FieldLabel htmlFor="provider-api-env">Credential environment</FieldLabel><Input id="provider-api-env" value={form.bearerTokenEnv} onChange={(event) => setForm({ ...form, bearerTokenEnv: event.target.value })} /></Field>
          <Field className="sm:col-span-2" orientation="horizontal"><Checkbox id="clear-token" checked={form.clearBearerToken} onCheckedChange={(checked: boolean | "indeterminate") => setForm({ ...form, clearBearerToken: checked === true, bearerToken: checked === true ? "" : form.bearerToken })} /><FieldLabel htmlFor="clear-token">Clear saved API key</FieldLabel></Field>
          <Field className="sm:col-span-2"><FieldLabel htmlFor="provider-headers">HTTP headers (write-only JSON)</FieldLabel><Textarea id="provider-headers" className="min-h-24 font-mono text-xs" value={form.headersText} disabled={form.clearHeaders} onChange={(event) => setForm({ ...form, headersText: event.target.value })} placeholder={existing?.has_http_headers ? "Leave blank to keep existing headers" : '{"x-provider-feature":"value"}'} /></Field>
          <Field className="sm:col-span-2" orientation="horizontal"><Checkbox id="clear-headers" checked={form.clearHeaders} onCheckedChange={(checked: boolean | "indeterminate") => setForm({ ...form, clearHeaders: checked === true, headersText: checked === true ? "" : form.headersText })} /><FieldLabel htmlFor="clear-headers">Clear saved headers</FieldLabel></Field>
        </> : <Field className="sm:col-span-2"><FieldLabel htmlFor="custom-provider-config">PL ProviderConfig JSON</FieldLabel><Textarea id="custom-provider-config" className="min-h-96 font-mono text-xs" value={form.customConfig} onChange={(event) => setForm({ ...form, customConfig: event.target.value })} /><FieldDescription>The server passes this object directly to PL validation. Include bearer_token or http_headers here only when setting or clearing them.</FieldDescription></Field>}
      </FieldGroup>{error && <Alert variant="destructive" className="mt-4"><AlertDescription>{error}</AlertDescription></Alert>}</div>
      <SheetFooter className="border-t"><Button variant="outline" onClick={onClose}>Cancel</Button><Button disabled={saving} onClick={() => void submit()}>{saving ? <Spinner data-icon="inline-start" /> : existing ? <Check data-icon="inline-start" /> : <Plus data-icon="inline-start" />}{saving ? "Saving…" : "Save provider"}</Button></SheetFooter>
    </SheetContent>
  </Sheet>
}

function formFromProvider(catalog: ProviderCatalog, response: ProvidersResponse, provider: ProviderInstance | null, index: number | "new"): ProviderForm {
  if (!provider) {
    const preset = catalog.presets[0]
    return {
      source: preset ? "preset" : "custom",
      id: preset ? uniqueProviderId(preset.id, response.providers, index) : "custom",
      presetId: preset?.id || "",
      name: preset?.display_name || "Custom Provider",
      baseUrl: preset?.base_url || "",
      bearerToken: "",
      clearBearerToken: false,
      bearerTokenEnv: preset?.credential?.env_var || "",
      headersText: "",
      clearHeaders: false,
      customConfig: "{}",
    }
  }
  const presetId = provider.config.preset || ""
  return {
    source: presetId ? "preset" : "custom",
    id: provider.id,
    presetId,
    name: provider.config.name,
    baseUrl: provider.config.base_url,
    bearerToken: "",
    clearBearerToken: false,
    bearerTokenEnv: provider.config.bearer_token_env || "",
    headersText: "",
    clearHeaders: false,
    customConfig: JSON.stringify(provider.config, null, 2),
  }
}

function providerRequest(provider: ProviderInstance): ProviderRequestPayload {
  if (provider.config.preset) {
    return {
      id: provider.id,
      source: "preset",
      preset_id: provider.config.preset,
      name: provider.config.name,
      base_url: provider.config.base_url,
      bearer_token_env: provider.config.bearer_token_env,
    }
  }
  return { id: provider.id, source: "custom", config: provider.config }
}

function customTemplate(form: ProviderForm) {
  return JSON.stringify({ name: form.name || "Custom Provider", base_url: form.baseUrl, capabilities: { source: "preset_defaults" }, catalog: { source: "explicit", models: [] } }, null, 2)
}

function parseProviderConfig(value: string): { value?: PlProviderConfig; error?: string } {
  try {
    const parsed = JSON.parse(value) as unknown
    if (!parsed || Array.isArray(parsed) || typeof parsed !== "object") return { error: "PL ProviderConfig JSON must be an object." }
    return { value: parsed as PlProviderConfig }
  } catch (error) {
    return { error: error instanceof Error ? error.message : "Invalid PL ProviderConfig JSON" }
  }
}

function parseHeaders(value: string): { value?: Record<string, string>; error?: string } {
  if (!value.trim()) return {}
  try {
    const parsed = JSON.parse(value) as unknown
    if (!parsed || Array.isArray(parsed) || typeof parsed !== "object") return { error: "HTTP headers JSON must be an object." }
    const entries = Object.entries(parsed)
    if (entries.some(([name, header]) => !name.trim() || typeof header !== "string")) return { error: "HTTP header names must be non-empty and values must be strings." }
    return { value: Object.fromEntries(entries) as Record<string, string> }
  } catch (error) {
    return { error: error instanceof Error ? error.message : "Invalid HTTP headers JSON" }
  }
}

function uniqueProviderId(base: string, providers: ProviderInstance[], editing: number | "new") {
  const reserved = new Set(providers.filter((_, index) => index !== editing).map((provider) => provider.id))
  if (!reserved.has(base)) return base
  let suffix = 2
  while (reserved.has(`${base}-${suffix}`)) suffix += 1
  return `${base}-${suffix}`
}
