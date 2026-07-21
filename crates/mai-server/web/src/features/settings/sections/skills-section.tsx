import { useQuery, useQueryClient } from "@tanstack/react-query"
import { FolderSearch, RefreshCw } from "lucide-react"
import { useEffect, useMemo, useState } from "react"
import { toast } from "sonner"

import { api, jsonBody } from "@/api/client"
import type { SkillsResponse } from "@/api/product-types"
import { queryKeys } from "@/api/queries"
import { EmptyState, ErrorState, LoadingState } from "@/components/page-state"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Avatar, AvatarFallback } from "@/components/ui/avatar"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Field, FieldContent, FieldDescription, FieldTitle } from "@/components/ui/field"
import { Switch } from "@/components/ui/switch"
import { SettingsBody, SettingsHeader, StickyActions } from "@/features/settings/settings-section"

export function SkillsSection() {
  const queryClient = useQueryClient()
  const query = useQuery({ queryKey: queryKeys.skills, queryFn: () => api<SkillsResponse>("/skills") })
  const initial = useMemo(() => Object.fromEntries((query.data?.skills ?? []).map((skill) => [skill.path, skill.enabled])), [query.data])
  const [enabled, setEnabled] = useState<Record<string, boolean>>({})
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState("")
  useEffect(() => setEnabled(initial), [initial])
  if (query.isLoading) return <LoadingState rows={7} />
  if (query.error) return <ErrorState error={query.error} retry={() => void query.refetch()} />
  const dirty = JSON.stringify(enabled) !== JSON.stringify(initial)
  const save = async () => {
    setSaving(true); setError("")
    try {
      await api<SkillsResponse>("/skills/config", { method: "PUT", ...jsonBody({ config: (query.data?.skills ?? []).map((skill) => ({ name: skill.name, path: skill.path, enabled: enabled[skill.path] ?? false })) }) })
      await queryClient.invalidateQueries({ queryKey: queryKeys.skills })
      toast.success("Skill configuration saved")
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)) } finally { setSaving(false) }
  }
  const active = Object.values(enabled).filter(Boolean).length

  return <>
    <SettingsHeader title="Skills" description="Codex-compatible skills discovered from repository, user, and system roots." status={<Badge variant="secondary">{active} enabled</Badge>} />
    <SettingsBody>
      <div className="mb-5 flex flex-wrap items-center justify-between gap-3"><div className="flex flex-wrap gap-2">{query.data?.roots.map((root) => <Badge key={root} variant="secondary">{root}</Badge>)}</div><Button variant="outline" onClick={() => void query.refetch()}><RefreshCw data-icon="inline-start" /> Reload</Button></div>
      <div className="mb-3 flex flex-col gap-2">{query.data?.errors.map((item) => <Alert key={item.path} variant="destructive"><AlertTitle>{item.path}</AlertTitle><AlertDescription>{item.message}</AlertDescription></Alert>)}</div>
      {!query.data?.skills.length ? <EmptyState title="No skills found" description="Reload after adding a skill to a configured discovery root." /> : <div className="divide-y rounded-lg border">
        {query.data?.skills.map((skill) => <div key={skill.path} className="flex items-start gap-3 px-4 py-3">
          <Avatar className="mt-0.5 size-8 rounded-md"><AvatarFallback className="rounded-md"><FolderSearch className="size-4" /></AvatarFallback></Avatar>
          <Field orientation="horizontal"><FieldContent><FieldTitle>{skill.name} <Badge variant="outline">{skill.scope}</Badge></FieldTitle><FieldDescription>{skill.description}<code className="block truncate text-xs">{skill.path}</code></FieldDescription></FieldContent><Switch checked={enabled[skill.path] ?? false} onCheckedChange={(checked: boolean) => setEnabled({ ...enabled, [skill.path]: checked })} aria-label={`Enable ${skill.name}`} /></Field>
        </div>)}
      </div>}
    </SettingsBody>
    <StickyActions dirty={dirty} saving={saving} onReset={() => setEnabled(initial)} onSave={() => void save()} error={error} />
  </>
}
