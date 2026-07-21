import { useQuery } from "@tanstack/react-query"
import { AtSign, X } from "lucide-react"
import { useMemo } from "react"

import { api } from "@/api/client"
import type { SkillSummary, SkillsResponse } from "@/api/product-types"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { DropdownMenu, DropdownMenuCheckboxItem, DropdownMenuContent, DropdownMenuGroup, DropdownMenuLabel, DropdownMenuSeparator, DropdownMenuTrigger } from "@/components/ui/dropdown-menu"

export function SkillMentionPicker({ endpoint, selected, onChange }: { endpoint: string; selected: string[]; onChange(next: string[]): void }) {
  const query = useQuery({
    queryKey: ["composer-skills", endpoint],
    queryFn: () => api<SkillsResponse>(endpoint),
    staleTime: 30_000,
  })
  const nameCounts = useMemo(() => {
    const counts = new Map<string, number>()
    for (const skill of query.data?.skills ?? []) counts.set(skill.name, (counts.get(skill.name) ?? 0) + 1)
    return counts
  }, [query.data])
  const skillsByMention = new Map((query.data?.skills ?? []).map((skill) => [mentionValue(skill, nameCounts), skill]))

  const toggle = (skill: SkillSummary) => {
    const value = mentionValue(skill, nameCounts)
    onChange(selected.includes(value) ? selected.filter((item) => item !== value) : [...selected, value])
  }

  return <>
    {selected.length > 0 && <div className="flex flex-wrap gap-1.5">{selected.map((value) => <Badge key={value} variant="secondary">{skillsByMention.get(value)?.name || value}<button type="button" aria-label={`Remove ${value}`} onClick={() => onChange(selected.filter((item) => item !== value))}><X className="size-3" /></button></Badge>)}</div>}
    <DropdownMenu>
      <DropdownMenuTrigger asChild><Button variant="ghost" size="sm" className="shrink-0" aria-label="Select skills"><AtSign data-icon="inline-start" /> Skills{selected.length > 0 && <Badge variant="secondary">{selected.length}</Badge>}</Button></DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="max-h-80 w-80 overflow-y-auto">
        <DropdownMenuLabel>Skills</DropdownMenuLabel><DropdownMenuSeparator />
        <DropdownMenuGroup>
          {query.isLoading && <div className="px-2 py-4 text-center text-xs text-muted-foreground">Loading skills…</div>}
          {query.error && <div className="px-2 py-4 text-xs text-destructive">{query.error.message}</div>}
          {(query.data?.skills ?? []).filter((skill) => skill.enabled).map((skill) => {
            const value = mentionValue(skill, nameCounts)
            return <DropdownMenuCheckboxItem key={skill.path} checked={selected.includes(value)} onSelect={(event: Event) => { event.preventDefault(); toggle(skill) }}><span className="min-w-0"><strong className="block truncate text-xs">{skill.name}</strong><small className="block truncate text-[10px] text-muted-foreground">{skill.description || skill.path}</small></span></DropdownMenuCheckboxItem>
          })}
          {query.data && !query.data.skills.some((skill) => skill.enabled) && <div className="px-2 py-4 text-center text-xs text-muted-foreground">No enabled skills.</div>}
        </DropdownMenuGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  </>
}

function mentionValue(skill: SkillSummary, counts: Map<string, number>) {
  return skill.scope === "project" || (counts.get(skill.name) ?? 0) > 1 ? skill.path : skill.name
}
