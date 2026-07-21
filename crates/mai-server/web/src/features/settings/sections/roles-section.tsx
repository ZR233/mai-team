import { useQuery, useQueryClient } from "@tanstack/react-query"
import { Bot, Compass, Hammer, ShieldCheck } from "lucide-react"
import { useEffect, useId, useMemo, useState } from "react"
import { Link } from "react-router-dom"
import { toast } from "sonner"

import { api, jsonBody } from "@/api/client"
import type { AgentConfigResponse, AgentModelPreference, ProviderInstance } from "@/api/product-types"
import { providersQuery, queryKeys } from "@/api/queries"
import { EmptyState, ErrorState, LoadingState } from "@/components/page-state"
import { StatusBadge } from "@/components/status"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { Avatar, AvatarFallback } from "@/components/ui/avatar"
import { Button } from "@/components/ui/button"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { SettingsBody, SettingsHeader, StickyActions } from "@/features/settings/settings-section"
import { cn } from "@/lib/utils"

const roles = [
  { id: "planner", label: "Planner", description: "Decomposes work and coordinates dependencies.", icon: Bot },
  { id: "explorer", label: "Explorer", description: "Explores code, docs, and web context.", icon: Compass },
  { id: "executor", label: "Executor", description: "Writes code, runs commands, and delivers work.", icon: Hammer },
  { id: "reviewer", label: "Reviewer", description: "Reviews changes, tests quality, and reports findings.", icon: ShieldCheck },
] as const

type RoleId = typeof roles[number]["id"]
type RoleForm = Record<RoleId, AgentModelPreference>

export function RolesSection() {
  const queryClient = useQueryClient()
  const providers = useQuery(providersQuery())
  const config = useQuery({ queryKey: queryKeys.agentConfig, queryFn: () => api<AgentConfigResponse>("/agent-config") })
  const initial = useMemo(() => buildForm(config.data, providers.data?.providers ?? []), [config.data, providers.data])
  const [form, setForm] = useState<RoleForm | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState("")
  useEffect(() => { setForm(initial) }, [initial])

  if (providers.isLoading || config.isLoading || !form) return <LoadingState rows={7} />
  if (providers.error) return <ErrorState error={providers.error} retry={() => void providers.refetch()} />
  if (config.error) return <ErrorState error={config.error} retry={() => void config.refetch()} />
  const instances = providers.data?.providers ?? []
  if (instances.length === 0) return <><SettingsHeader title="Role Models" description="Assign a provider and model to each runtime role." /><SettingsBody><EmptyState title="No providers configured" description="Create a provider instance before assigning role routes." action={<Button asChild><Link to="/providers">Open Providers</Link></Button>} /></SettingsBody></>
  const dirty = JSON.stringify(form) !== JSON.stringify(initial)

  const save = async () => {
    setSaving(true); setError("")
    try {
      await api<AgentConfigResponse>("/agent-config", { method: "PUT", ...jsonBody(form) })
      await queryClient.invalidateQueries({ queryKey: queryKeys.agentConfig })
      toast.success("Role models saved")
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)) } finally { setSaving(false) }
  }

  return <>
    <SettingsHeader title="Role Models" description="Assign model profiles to planner, explorer, executor, and reviewer roles." status={<StatusBadge status={roles.every(({ id }) => form[id].provider_id && form[id].model) ? "ready" : "incomplete"} />} />
    <SettingsBody>
      <div className="hidden overflow-hidden rounded-lg border md:block"><Table><TableHeader><TableRow><TableHead className="w-[42%]">Role & responsibility</TableHead><TableHead>Model profile</TableHead></TableRow></TableHeader><TableBody>
        {roles.map(({ id, label, description, icon: Icon }) => {
          const provider = instances.find((candidate) => candidate.id === form[id].provider_id) ?? instances[0]
          const model = provider?.models.find((candidate) => candidate.id === form[id].model)
          return <TableRow key={id}>
            <TableCell><RoleIdentity label={label} description={description} icon={Icon} provider={provider} model={model} preference={form[id]} /></TableCell>
            <TableCell><RoleSelects preference={form[id]} instances={instances} onChange={(preference) => setForm({ ...form, [id]: preference })} /></TableCell>
          </TableRow>
        })}
      </TableBody></Table></div>
      <div className="grid gap-3 md:hidden">
        {roles.map(({ id, label, description, icon }) => {
          const provider = instances.find((candidate) => candidate.id === form[id].provider_id) ?? instances[0]
          const model = provider?.models.find((candidate) => candidate.id === form[id].model)
          return <section key={id} className="rounded-lg border p-4"><RoleIdentity label={label} description={description} icon={icon} provider={provider} model={model} preference={form[id]} /><RoleSelects className="mt-4" preference={form[id]} instances={instances} onChange={(preference) => setForm({ ...form, [id]: preference })} /></section>
        })}
      </div>
      {config.data?.validation_error && <Alert variant="destructive" className="mt-3"><AlertDescription>{config.data.validation_error}</AlertDescription></Alert>}
    </SettingsBody>
    <StickyActions dirty={dirty} saving={saving} onReset={() => setForm(initial)} onSave={() => void save()} error={error} />
  </>
}

function RoleIdentity({ label, description, icon: Icon, provider, model, preference }: {
  label: string
  description: string
  icon: typeof Bot
  provider?: ProviderInstance
  model?: ProviderInstance["models"][number]
  preference: AgentModelPreference
}) {
  return <div className="flex items-start gap-3"><Avatar className="size-9 rounded-lg"><AvatarFallback className="rounded-lg"><Icon className="size-4" /></AvatarFallback></Avatar><span className="min-w-0"><strong className="text-sm">{label}</strong><small className="mt-1 block text-xs leading-5 text-muted-foreground">{description}</small><span className="mt-1 block truncate font-mono text-[11px] text-muted-foreground">{provider?.name} · {model?.name || model?.display_name || model?.id} · {preference.reasoning_effort || reasoningDefault(model) || "default"}</span></span></div>
}

function RoleSelects({ preference, instances, onChange, className }: {
  preference: AgentModelPreference
  instances: ProviderInstance[]
  onChange(preference: AgentModelPreference): void
  className?: string
}) {
  const fieldId = useId()
  const provider = instances.find((candidate) => candidate.id === preference.provider_id) ?? instances[0]
  const model = provider?.models.find((candidate) => candidate.id === preference.model)
  const reasoning = reasoningOptions(model)
  const changeProvider = (providerId: string) => {
    const next = instances.find((candidate) => candidate.id === providerId)
    const modelId = next?.models.some((candidate) => candidate.id === preference.model) ? preference.model : next?.default_model || next?.models[0]?.id || ""
    onChange({ provider_id: providerId, model: modelId, reasoning_effort: reasoningDefault(next?.models.find((candidate) => candidate.id === modelId)) })
  }
  const changeModel = (modelId: string) => onChange({ ...preference, model: modelId, reasoning_effort: reasoningDefault(provider?.models.find((candidate) => candidate.id === modelId)) })
  return <FieldGroup className={cn("gap-3 md:grid md:grid-cols-3", className)}>
    <Field><FieldLabel htmlFor={`${fieldId}-provider`}>Provider</FieldLabel><Select value={provider?.id} onValueChange={changeProvider}><SelectTrigger id={`${fieldId}-provider`} className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectGroup>{instances.map((item) => <SelectItem key={item.id} value={item.id}>{item.name}</SelectItem>)}</SelectGroup></SelectContent></Select></Field>
    <Field><FieldLabel htmlFor={`${fieldId}-model`}>Model</FieldLabel><Select value={preference.model} onValueChange={changeModel}><SelectTrigger id={`${fieldId}-model`} className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectGroup>{provider?.models.map((item) => <SelectItem key={item.id} value={item.id}>{item.name || item.display_name || item.id}</SelectItem>)}</SelectGroup></SelectContent></Select></Field>
    <Field><FieldLabel htmlFor={`${fieldId}-reasoning`}>Reasoning</FieldLabel>{reasoning.length > 0 ? <Select value={preference.reasoning_effort || "default"} onValueChange={(value: string) => onChange({ ...preference, reasoning_effort: value === "default" ? null : value })}><SelectTrigger id={`${fieldId}-reasoning`} className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectGroup><SelectItem value="default">Model default</SelectItem>{reasoning.map((value) => <SelectItem key={value} value={value}>{value}</SelectItem>)}</SelectGroup></SelectContent></Select> : <div className="flex h-8 items-center text-xs text-muted-foreground">Not configurable</div>}</Field>
  </FieldGroup>
}

function buildForm(config: AgentConfigResponse | undefined, providers: ProviderInstance[]): RoleForm | null {
  if (!config && providers.length === 0) return null
  const fallback = providers[0]
  const preference = (role: RoleId): AgentModelPreference => config?.[role] ?? {
    provider_id: fallback?.id ?? "",
    model: fallback?.default_model ?? fallback?.models[0]?.id ?? "",
    reasoning_effort: null,
  }
  return { planner: preference("planner"), explorer: preference("explorer"), executor: preference("executor"), reviewer: preference("reviewer") }
}

function reasoningOptions(model?: ProviderInstance["models"][number]) {
  return model?.reasoning?.candidates ?? model?.reasoning?.variants?.map((variant) => variant.id) ?? []
}

function reasoningDefault(model?: ProviderInstance["models"][number]) {
  return model?.reasoning?.default ?? model?.reasoning?.default_variant ?? null
}
