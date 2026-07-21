import { useQuery, useQueryClient } from "@tanstack/react-query"
import { Check, LoaderCircle, Pencil, Plus, TestTube2, Trash2 } from "lucide-react"
import { useMemo, useState } from "react"
import { toast } from "sonner"

import { api, jsonBody } from "@/api/client"
import type { ProviderCatalog, ProviderInstance, ProviderModel, ProvidersResponse, ProviderTestResponse } from "@/api/product-types"
import { providerCatalogQuery, providersQuery, queryKeys } from "@/api/queries"
import { EmptyState, ErrorState, LoadingState } from "@/components/page-state"
import { StatusBadge } from "@/components/status"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle, AlertDialogTrigger } from "@/components/ui/alert-dialog"
import { Avatar, AvatarFallback } from "@/components/ui/avatar"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Field, FieldContent, FieldDescription, FieldGroup, FieldLabel, FieldLegend, FieldSet, FieldTitle } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Sheet, SheetContent, SheetDescription, SheetFooter, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { Spinner } from "@/components/ui/spinner"
import { Switch } from "@/components/ui/switch"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { Textarea } from "@/components/ui/textarea"
import { WorkspaceHeader } from "@/components/workspace-header"

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

  const saveAll = async (nextProviders: ProviderRequestPayload[], defaultProviderId: string | null) => {
    await api<ProvidersResponse>("/providers", { method: "PUT", ...jsonBody({ providers: nextProviders, default_provider_id: defaultProviderId }) })
    await queryClient.invalidateQueries({ queryKey: queryKeys.providers })
  }
  const remove = async (index: number) => {
    const next = providers.data.providers.filter((_, candidate) => candidate !== index).map(providerRequest)
    const currentDefault = providers.data.default_provider_id ?? null
    const defaultId = next.some((provider) => provider.id === currentDefault) ? currentDefault ?? null : next[0]?.id ?? null
    try { await saveAll(next, defaultId); toast.success("Provider removed") } catch (error) { toast.error(error instanceof Error ? error.message : String(error)) }
  }
  const testProvider = async (provider: ProviderInstance) => {
    setTestingId(provider.id)
    try {
      const result = await api<ProviderTestResponse>(`/providers/${encodeURIComponent(provider.id)}/test`, {
        method: "POST",
        ...jsonBody({ model: provider.default_model, reasoning_effort: null, deep: true }),
      })
      toast.success(`${provider.name} connected in ${result.latency_ms} ms`, {
        description: result.output_preview || `${result.transport.protocol} · ${result.model}`,
      })
    } catch (error) {
      toast.error(`${provider.name} connection failed`, {
        description: error instanceof Error ? error.message : String(error),
      })
    } finally {
      setTestingId(null)
    }
  }

  return (
    <div className="flex h-full min-h-0 flex-col bg-background">
      <WorkspaceHeader crumbs={[{ label: "Providers" }]} actions={<Button onClick={() => setEditing("new")}><Plus data-icon="inline-start" /> Add provider</Button>} />
      <div className="min-h-0 flex-1 overflow-auto"><div className="mx-auto flex max-w-6xl flex-col gap-5 p-4 md:p-6">
        <div><h2 className="text-base font-semibold">Provider instances</h2><p className="text-sm text-muted-foreground">Credentials, transports, and model catalogs resolved by PL.</p></div>
        {providers.data.providers.length === 0
          ? <EmptyState title="No providers configured" description="Create an instance from the PL catalog or add a custom compatible endpoint." action={<Button onClick={() => setEditing("new")}><Plus data-icon="inline-start" /> Add provider</Button>} />
          : <div className="overflow-hidden rounded-lg border"><Table><TableHeader><TableRow><TableHead>Provider</TableHead><TableHead className="hidden md:table-cell">Transport</TableHead><TableHead className="hidden lg:table-cell">Default model</TableHead><TableHead>Status</TableHead><TableHead className="w-32 text-right">Actions</TableHead></TableRow></TableHeader><TableBody>{providers.data.providers.map((provider, index) => <TableRow key={provider.id}>
            <TableCell><div className="flex items-center gap-3"><Avatar className="size-9 rounded-lg"><AvatarFallback className="rounded-lg">{provider.name.slice(0, 1).toUpperCase()}</AvatarFallback></Avatar><div className="min-w-0"><div className="flex items-center gap-2"><span className="font-medium">{provider.name}</span>{providers.data.default_provider_id === provider.id && <Badge variant="secondary">Default</Badge>}</div><div className="max-w-72 truncate text-xs text-muted-foreground">{provider.id} · {provider.base_url}</div></div></div></TableCell>
            <TableCell className="hidden md:table-cell"><span className="text-sm">{String(provider.transport?.protocol || "compatible")}</span><span className="block text-xs text-muted-foreground">{String(provider.transport?.connection_mode || "http")}</span></TableCell>
            <TableCell className="hidden font-medium lg:table-cell">{provider.default_model}</TableCell>
            <TableCell><StatusBadge status={provider.enabled ? "active" : "disabled"} /></TableCell>
            <TableCell><div className="flex justify-end gap-1"><Button variant="ghost" size="icon" disabled={testingId === provider.id} onClick={() => void testProvider(provider)} aria-label={`Test ${provider.name}`}>{testingId === provider.id ? <LoaderCircle data-icon="inline-start" className="animate-spin" /> : <TestTube2 data-icon="inline-start" />}</Button><Button variant="ghost" size="icon" onClick={() => setEditing(index)} aria-label={`Edit ${provider.name}`}><Pencil data-icon="inline-start" /></Button><AlertDialog><AlertDialogTrigger asChild><Button variant="ghost" size="icon" aria-label={`Delete ${provider.name}`}><Trash2 data-icon="inline-start" /></Button></AlertDialogTrigger><AlertDialogContent><AlertDialogHeader><AlertDialogTitle>Delete {provider.name}?</AlertDialogTitle><AlertDialogDescription>Roles using this provider must be reassigned before they can run.</AlertDialogDescription></AlertDialogHeader><AlertDialogFooter><AlertDialogCancel>Cancel</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={() => void remove(index)}>Delete provider</AlertDialogAction></AlertDialogFooter></AlertDialogContent></AlertDialog></div></TableCell>
          </TableRow>)}</TableBody></Table></div>}
      </div></div>
      {editing !== null && <ProviderEditor
        catalog={catalog.data}
        response={providers.data}
        index={editing}
        onClose={() => setEditing(null)}
        onSave={async (provider, makeDefault) => {
          const next = providers.data.providers.map(providerRequest)
          if (editing === "new") next.push(provider)
          else next[editing] = provider
          const defaultId = makeDefault ? String(provider.id) : providers.data.default_provider_id || next[0]?.id || null
          await saveAll(next, defaultId)
          setEditing(null)
          toast.success("Provider saved")
        }}
      />}
    </div>
  )
}

function ProvidersFrame({ children }: { children: React.ReactNode }) {
  return <div className="flex h-full min-h-0 flex-col"><WorkspaceHeader crumbs={[{ label: "Providers" }]} /><div className="min-h-0 flex-1 overflow-auto">{children}</div></div>
}

interface ProviderForm {
  id: string
  presetId: string
  protocol: string
  connectionMode: string
  name: string
  baseUrl: string
  apiKey: string
  apiKeyEnv: string
  headersText: string
  clearHeaders: boolean
  catalogSource: "bundled" | "explicit"
  catalogId: string
  defaultModel: string
  customModels: string
  capabilitySource: "preset_defaults" | "explicit"
  hostedSearch: boolean
  standaloneSearch: string
  enabled: boolean
  makeDefault: boolean
}

function ProviderEditor({ catalog, response, index, onClose, onSave }: {
  catalog: ProviderCatalog
  response: ProvidersResponse
  index: number | "new"
  onClose(): void
  onSave(provider: ProviderRequestPayload, makeDefault: boolean): Promise<void>
}) {
  const existing = index === "new" ? null : response.providers[index]
  const initial = useMemo(() => formFromProvider(catalog, response, existing, index), [catalog, existing, index, response])
  const [form, setForm] = useState(initial)
  const [error, setError] = useState("")
  const [saving, setSaving] = useState(false)
  const preset = catalog.presets.find((candidate) => candidate.id === form.presetId)
  const modes = preset?.transport.connection_modes ?? (form.protocol === "responses" ? [{ id: "web_socket", display_name: "WebSocket" }, { id: "http", display_name: "HTTP" }] : [{ id: "http", display_name: "HTTP" }])
  const bundledModels = form.catalogSource === "bundled" ? catalog.model_catalogs[form.catalogId]?.models ?? [] : []
  const customModels = parseModels(form.customModels)
  const headers = parseHeaders(form.headersText)
  const effectiveModels = [...bundledModels, ...(customModels.value ?? [])]

  const choosePreset = (id: string) => {
    if (id === "custom") {
      setForm((current) => ({ ...current, presetId: "", protocol: "chat_completions", connectionMode: "http", catalogSource: "explicit", catalogId: "", capabilitySource: "explicit", hostedSearch: false, standaloneSearch: "" }))
      return
    }
    const next = catalog.presets.find((candidate) => candidate.id === id)
    if (!next) return
    setForm((current) => ({
      ...current,
      id: uniqueProviderId(next.id, response.providers, index),
      presetId: next.id,
      protocol: next.transport.protocol,
      connectionMode: next.transport.default_connection_mode,
      name: next.display_name,
      baseUrl: next.base_url,
      apiKeyEnv: next.credential?.env_var || "",
      catalogSource: "bundled",
      catalogId: next.model_catalog_id,
      defaultModel: next.suggested_model,
      customModels: "[]",
      headersText: "",
      clearHeaders: false,
      capabilitySource: "preset_defaults",
    }))
  }
  const submit = async () => {
    setError("")
    if (!form.id.trim() || !form.name.trim() || !form.baseUrl.trim() || !form.defaultModel) return setError("Provider ID, name, base URL, and default model are required.")
    if (customModels.error) return setError(customModels.error)
    if (headers.error) return setError(headers.error)
    if (!effectiveModels.some((model) => model.id === form.defaultModel)) return setError("Default model must exist in the effective model catalog.")
    const provider = {
      id: form.id.trim(),
      preset_id: form.presetId || null,
      transport: { protocol: form.protocol, connection_mode: form.connectionMode },
      capabilities: form.capabilitySource === "preset_defaults" ? { source: "preset_defaults" } : { source: "explicit", web_search: { hosted_responses: form.hostedSearch, standalone: form.standaloneSearch || null } },
      name: form.name.trim(),
      base_url: form.baseUrl.trim(),
      api_key: form.apiKey,
      api_key_env: form.apiKeyEnv || null,
      http_headers: form.clearHeaders ? {} : headers.value ?? null,
      catalog: form.catalogSource === "bundled" ? { source: "bundled", catalog_id: form.catalogId, additional_models: customModels.value } : { source: "explicit", models: customModels.value },
      default_model: form.defaultModel,
      enabled: form.enabled,
    }
    setSaving(true)
    try { await onSave(provider, form.makeDefault) } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)) } finally { setSaving(false) }
  }

  return (
    <Sheet open onOpenChange={(open: boolean) => { if (!open) onClose() }}>
      <SheetContent className="w-full gap-0 sm:max-w-2xl">
        <SheetHeader><SheetTitle>{existing ? `Edit ${existing.name}` : "Add provider"}</SheetTitle><SheetDescription>Instances are independent even when they use the same preset. Catalog fields come from PL.</SheetDescription></SheetHeader>
        <div className="min-h-0 flex-1 overflow-y-auto px-4 py-2">
          <FieldGroup className="grid sm:grid-cols-2">
            <Field><FieldLabel>Preset</FieldLabel><Select value={form.presetId || "custom"} onValueChange={choosePreset}><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectGroup>{catalog.presets.map((item) => <SelectItem key={item.id} value={item.id}>{item.display_name}</SelectItem>)}<SelectItem value="custom">Custom compatible provider</SelectItem></SelectGroup></SelectContent></Select></Field>
            <Field><FieldLabel htmlFor="provider-id">Provider ID</FieldLabel><Input id="provider-id" value={form.id} onChange={(event) => setForm({ ...form, id: event.target.value })} /></Field>
            <Field><FieldLabel htmlFor="provider-name">Display name</FieldLabel><Input id="provider-name" value={form.name} onChange={(event) => setForm({ ...form, name: event.target.value })} /></Field>
            <Field><FieldLabel htmlFor="provider-url">Base URL</FieldLabel><Input id="provider-url" value={form.baseUrl} onChange={(event) => setForm({ ...form, baseUrl: event.target.value })} /></Field>
            {!preset && <Field><FieldLabel>Wire protocol</FieldLabel><Select value={form.protocol} onValueChange={(protocol: string) => setForm({ ...form, protocol, connectionMode: protocol === "chat_completions" ? "http" : form.connectionMode })}><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectGroup><SelectItem value="responses">Responses</SelectItem><SelectItem value="chat_completions">Chat Completions</SelectItem></SelectGroup></SelectContent></Select></Field>}
            <Field><FieldLabel>Connection mode</FieldLabel><Tabs value={form.connectionMode} onValueChange={(connectionMode: string) => setForm({ ...form, connectionMode })}><TabsList className="w-full">{modes.map((mode) => <TabsTrigger key={mode.id} className="flex-1" value={mode.id}>{mode.display_name}</TabsTrigger>)}</TabsList></Tabs></Field>
            <Field><FieldLabel htmlFor="provider-api-key">{preset?.credential?.label || "API key"}</FieldLabel><Input id="provider-api-key" type="password" value={form.apiKey} onChange={(event) => setForm({ ...form, apiKey: event.target.value })} placeholder={existing?.has_api_key ? "Leave blank to keep existing" : "Optional when env is set"} /><FieldDescription>Write-only. A blank value keeps the saved secret.</FieldDescription></Field>
            <Field><FieldLabel htmlFor="provider-api-env">Credential environment</FieldLabel><Input id="provider-api-env" value={form.apiKeyEnv} onChange={(event) => setForm({ ...form, apiKeyEnv: event.target.value })} /></Field>
            <Field><FieldLabel>Default model</FieldLabel><Select value={form.defaultModel} onValueChange={(defaultModel: string) => setForm({ ...form, defaultModel })}><SelectTrigger className="w-full"><SelectValue placeholder="Select model" /></SelectTrigger><SelectContent><SelectGroup>{effectiveModels.map((model) => <SelectItem key={model.id} value={model.id}>{model.display_name || model.name || model.id}</SelectItem>)}</SelectGroup></SelectContent></Select></Field>
            <Field orientation="horizontal"><FieldContent><FieldTitle>Enabled</FieldTitle><FieldDescription>Make this provider available to role routes.</FieldDescription></FieldContent><Switch checked={form.enabled} onCheckedChange={(enabled: boolean) => setForm({ ...form, enabled })} /></Field>
            <Field orientation="horizontal"><Checkbox id="provider-default" checked={form.makeDefault} onCheckedChange={(checked: boolean | "indeterminate") => setForm({ ...form, makeDefault: checked === true })} /><FieldLabel htmlFor="provider-default">Make default provider</FieldLabel></Field>
            <Field className="sm:col-span-2"><FieldLabel htmlFor="provider-headers">HTTP headers (write-only JSON)</FieldLabel><Textarea id="provider-headers" className="min-h-20 font-mono text-xs" value={form.headersText} onChange={(event) => setForm({ ...form, headersText: event.target.value, clearHeaders: false })} placeholder={'{"x-provider-feature":"value"}'} /></Field>
            <Field orientation="horizontal" className="sm:col-span-2"><Checkbox id="clear-headers" checked={form.clearHeaders} onCheckedChange={(checked: boolean | "indeterminate") => setForm({ ...form, clearHeaders: checked === true, headersText: checked === true ? "" : form.headersText })} /><FieldLabel htmlFor="clear-headers">Clear saved headers</FieldLabel></Field>
            <Field className="sm:col-span-2"><FieldLabel htmlFor="provider-models">Additional/custom models (JSON)</FieldLabel><Textarea id="provider-models" className="min-h-28 font-mono text-xs" value={form.customModels} onChange={(event) => setForm({ ...form, customModels: event.target.value })} /></Field>
            <FieldSet className="rounded-lg border p-3 sm:col-span-2"><FieldLegend variant="label">Service capabilities</FieldLegend><FieldGroup>
              <Field orientation="responsive"><FieldLabel>Capability source</FieldLabel><Select value={form.capabilitySource} onValueChange={(capabilitySource: string) => setForm({ ...form, capabilitySource: capabilitySource as ProviderForm["capabilitySource"] })}><SelectTrigger className="w-full sm:w-48"><SelectValue /></SelectTrigger><SelectContent><SelectGroup><SelectItem value="preset_defaults">Preset defaults</SelectItem><SelectItem value="explicit">Explicit</SelectItem></SelectGroup></SelectContent></Select></Field>
              {form.capabilitySource === "explicit" && <><Field orientation="horizontal"><FieldContent><FieldTitle>Hosted Web Search</FieldTitle><FieldDescription>Allow the Responses hosted search tool.</FieldDescription></FieldContent><Switch checked={form.hostedSearch} onCheckedChange={(hostedSearch: boolean) => setForm({ ...form, hostedSearch })} /></Field><Field><FieldLabel htmlFor="standalone-search">Standalone dialect</FieldLabel><Input id="standalone-search" value={form.standaloneSearch} onChange={(event) => setForm({ ...form, standaloneSearch: event.target.value })} placeholder="Optional" /></Field></>}
            </FieldGroup></FieldSet>
          </FieldGroup>
          {error && <Alert variant="destructive" className="mt-4"><AlertDescription>{error}</AlertDescription></Alert>}
        </div>
        <SheetFooter className="border-t"><Button variant="outline" onClick={onClose}>Cancel</Button><Button disabled={saving} onClick={() => void submit()}>{saving ? <Spinner data-icon="inline-start" /> : existing ? <Check data-icon="inline-start" /> : <Plus data-icon="inline-start" />}{saving ? "Saving…" : "Save provider"}</Button></SheetFooter>
      </SheetContent>
    </Sheet>
  )
}

function formFromProvider(catalog: ProviderCatalog, response: ProvidersResponse, provider: ProviderInstance | null, index: number | "new"): ProviderForm {
  if (!provider) {
    const preset = catalog.presets[0]
    return {
      id: preset ? uniqueProviderId(preset.id, response.providers, index) : "custom",
      presetId: preset?.id || "",
      protocol: preset?.transport.protocol || "chat_completions",
      connectionMode: preset?.transport.default_connection_mode || "http",
      name: preset?.display_name || "Custom Provider",
      baseUrl: preset?.base_url || "",
      apiKey: "",
      apiKeyEnv: preset?.credential?.env_var || "",
      headersText: "",
      clearHeaders: false,
      catalogSource: preset ? "bundled" : "explicit",
      catalogId: preset?.model_catalog_id || "",
      defaultModel: preset?.suggested_model || "",
      customModels: "[]",
      capabilitySource: preset ? "preset_defaults" : "explicit",
      hostedSearch: false,
      standaloneSearch: "",
      enabled: true,
      makeDefault: response.providers.length === 0,
    }
  }
  const providerCatalog = provider.catalog as { source?: string; catalog_id?: string; additional_models?: ProviderModel[]; models?: ProviderModel[] } | undefined
  const selection = provider.capability_selection as { source?: string; web_search?: { hosted_responses?: boolean; standalone?: string } } | undefined
  return {
    id: provider.id,
    presetId: provider.preset_id || "",
    protocol: String(provider.transport?.protocol || "chat_completions"),
    connectionMode: String(provider.transport?.connection_mode || "http"),
    name: provider.name,
    baseUrl: provider.base_url || "",
    apiKey: "",
    apiKeyEnv: provider.api_key_env || "",
    headersText: "",
    clearHeaders: false,
    catalogSource: providerCatalog?.source === "bundled" ? "bundled" : "explicit",
    catalogId: providerCatalog?.catalog_id || "",
    defaultModel: provider.default_model || provider.models[0]?.id || "",
    customModels: JSON.stringify(providerCatalog?.source === "bundled" ? providerCatalog.additional_models ?? [] : providerCatalog?.models ?? [], null, 2),
    capabilitySource: selection?.source === "preset_defaults" ? "preset_defaults" : "explicit",
    hostedSearch: selection?.web_search?.hosted_responses === true,
    standaloneSearch: selection?.web_search?.standalone || "",
    enabled: provider.enabled,
    makeDefault: response.default_provider_id === provider.id,
  }
}

interface ProviderRequestPayload extends Record<string, unknown> {
  id: string
}

function providerRequest(provider: ProviderInstance): ProviderRequestPayload {
  return {
    id: provider.id,
    preset_id: provider.preset_id,
    transport: { protocol: provider.transport?.protocol, connection_mode: provider.transport?.connection_mode },
    capabilities: provider.capability_selection,
    name: provider.name,
    base_url: provider.base_url,
    api_key: "",
    api_key_env: provider.api_key_env,
    http_headers: null,
    catalog: provider.catalog,
    default_model: provider.default_model,
    enabled: provider.enabled,
  }
}

function parseModels(value: string): { value?: ProviderModel[]; error?: string } {
  try {
    const parsed = JSON.parse(value || "[]") as unknown
    if (!Array.isArray(parsed)) return { error: "Models JSON must be an array." }
    return { value: parsed as ProviderModel[] }
  } catch (error) { return { error: error instanceof Error ? error.message : "Invalid models JSON" } }
}

function parseHeaders(value: string): { value?: Record<string, string>; error?: string } {
  if (!value.trim()) return {}
  try {
    const parsed = JSON.parse(value) as unknown
    if (!parsed || Array.isArray(parsed) || typeof parsed !== "object") return { error: "HTTP headers JSON must be an object." }
    const entries = Object.entries(parsed)
    if (entries.some(([name, header]) => !name.trim() || typeof header !== "string")) return { error: "HTTP header names must be non-empty and values must be strings." }
    return { value: Object.fromEntries(entries) as Record<string, string> }
  } catch (error) { return { error: error instanceof Error ? error.message : "Invalid HTTP headers JSON" } }
}

function uniqueProviderId(base: string, providers: ProviderInstance[], editing: number | "new") {
  const reserved = new Set(providers.filter((_, index) => index !== editing).map((provider) => provider.id))
  if (!reserved.has(base)) return base
  let suffix = 2
  while (reserved.has(`${base}-${suffix}`)) suffix += 1
  return `${base}-${suffix}`
}
