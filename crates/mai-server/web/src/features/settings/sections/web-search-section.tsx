import { useQuery, useQueryClient } from "@tanstack/react-query"
import { Globe2 } from "lucide-react"
import { useEffect, useMemo, useState } from "react"
import { toast } from "sonner"

import { api, jsonBody } from "@/api/client"
import type { WebSearchSettingsResponse } from "@/api/product-types"
import { queryKeys } from "@/api/queries"
import { EmptyState, ErrorState, LoadingState } from "@/components/page-state"
import { StatusBadge } from "@/components/status"
import { Avatar, AvatarFallback } from "@/components/ui/avatar"
import { FieldGroup } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { Textarea } from "@/components/ui/textarea"
import { FormField, SettingsBody, SettingsHeader, StickyActions } from "@/features/settings/settings-section"

interface SearchForm {
  mode: string
  contextSize: string
  domains: string
  country: string
  region: string
  city: string
  timezone: string
}

export function WebSearchSection() {
  const queryClient = useQueryClient()
  const query = useQuery({ queryKey: queryKeys.webSearch, queryFn: () => api<WebSearchSettingsResponse>("/settings/web-search") })
  const initial = useMemo<SearchForm>(() => ({
    mode: query.data?.config.mode || "cached",
    contextSize: query.data?.config.context_size || "medium",
    domains: query.data?.config.allowed_domains.join("\n") || "",
    country: query.data?.config.location?.country || "",
    region: query.data?.config.location?.region || "",
    city: query.data?.config.location?.city || "",
    timezone: query.data?.config.location?.timezone || "",
  }), [query.data])
  const [form, setForm] = useState(initial)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState("")

  useEffect(() => setForm(initial), [initial])
  if (query.isLoading) return <LoadingState rows={6} />
  if (query.error) return <ErrorState error={query.error} retry={() => void query.refetch()} />

  const dirty = JSON.stringify(form) !== JSON.stringify(initial)
  const save = async () => {
    setSaving(true)
    setError("")
    try {
      await api("/settings/web-search", { method: "PUT", ...jsonBody({
        mode: form.mode,
        context_size: form.contextSize || null,
        allowed_domains: form.domains.split(/[\n,]/).map((domain) => domain.trim()).filter(Boolean),
        location: [form.country, form.region, form.city, form.timezone].some(Boolean) ? { country: form.country || null, region: form.region || null, city: form.city || null, timezone: form.timezone || null } : null,
      }) })
      await queryClient.invalidateQueries({ queryKey: queryKeys.webSearch })
      toast.success("Web Search settings saved")
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setSaving(false)
    }
  }

  const roles = Object.entries(query.data?.roles ?? {})
  return (
    <>
      <SettingsHeader title="Web Search" description="PL resolves each role to a hosted or standalone search path from provider capabilities." status={<StatusBadge status={form.mode === "disabled" ? "disabled" : "configured"} />} />
      <SettingsBody>
        <div className="grid gap-5 lg:grid-cols-[minmax(18rem,0.8fr)_minmax(28rem,1.2fr)]">
          <section className="rounded-lg border p-5">
            <FieldGroup>
              <FormField label="Search mode">
                <Select value={form.mode} onValueChange={(mode: string) => setForm({ ...form, mode })}><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectGroup><SelectItem value="disabled">Disabled</SelectItem><SelectItem value="cached">Cached</SelectItem><SelectItem value="indexed">Indexed</SelectItem><SelectItem value="live">Live</SelectItem></SelectGroup></SelectContent></Select>
              </FormField>
              <FormField label="Context size">
                <Select value={form.contextSize} onValueChange={(contextSize: string) => setForm({ ...form, contextSize })}><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectGroup><SelectItem value="low">Low</SelectItem><SelectItem value="medium">Medium</SelectItem><SelectItem value="high">High</SelectItem></SelectGroup></SelectContent></Select>
              </FormField>
              <FormField label="Allowed domains" htmlFor="web-search-domains" hint="One hostname per line. Empty allows all domains."><Textarea id="web-search-domains" className="min-h-28" value={form.domains} onChange={(event) => setForm({ ...form, domains: event.target.value })} placeholder="docs.rs&#10;github.com" /></FormField>
              <div className="grid gap-4 sm:grid-cols-2"><FormField label="Country"><Input value={form.country} onChange={(event) => setForm({ ...form, country: event.target.value })} /></FormField><FormField label="Region"><Input value={form.region} onChange={(event) => setForm({ ...form, region: event.target.value })} /></FormField><FormField label="City"><Input value={form.city} onChange={(event) => setForm({ ...form, city: event.target.value })} /></FormField><FormField label="Timezone"><Input value={form.timezone} onChange={(event) => setForm({ ...form, timezone: event.target.value })} placeholder="Asia/Shanghai" /></FormField></div>
            </FieldGroup>
          </section>

          <section className="overflow-hidden rounded-lg border">
            <div className="border-b bg-muted/40 px-4 py-3"><h2 className="text-sm font-semibold">Role resolution</h2><p className="text-xs text-muted-foreground">Resolved by PL from current role routes and effective capabilities.</p></div>
            {roles.length === 0 ? <EmptyState title="No role resolutions" description="Assign providers to runtime roles to resolve Web Search." /> : <Table><TableHeader><TableRow><TableHead>Role</TableHead><TableHead>Provider and model</TableHead><TableHead>Path</TableHead><TableHead className="text-right">Availability</TableHead></TableRow></TableHeader><TableBody>
              {roles.map(([role, resolution]) => <TableRow key={role}><TableCell><div className="flex items-center gap-2"><Avatar className="size-8 rounded-lg"><AvatarFallback className="rounded-lg"><Globe2 className="size-4" /></AvatarFallback></Avatar><strong className="capitalize">{role}</strong></div></TableCell><TableCell><div className="font-medium">{resolution.provider_id || "No provider"}</div><div className="text-xs text-muted-foreground">{resolution.model || "No model"}</div></TableCell><TableCell className="text-muted-foreground">{resolution.path || "Unavailable"}</TableCell><TableCell className="text-right"><StatusBadge status={resolution.availability} /></TableCell></TableRow>)}
            </TableBody></Table>}
          </section>
        </div>
      </SettingsBody>
      <StickyActions dirty={dirty} saving={saving} onReset={() => setForm(initial)} onSave={() => void save()} error={error} />
    </>
  )
}
