import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { Bot, GitBranch, Plus, RefreshCw, Save, Search, ShieldCheck, Trash2, Users } from "lucide-react"
import { useEffect, useState } from "react"
import { useNavigate, useParams, useSearchParams } from "react-router-dom"
import { toast } from "sonner"

import { api, jsonBody } from "@/api/client"
import type { ProjectDetail } from "@/api/product-types"
import { projectQuery, projectsQuery, queryKeys } from "@/api/queries"
import { EmptyState, ErrorState, LoadingState } from "@/components/page-state"
import { ResourceSidebar } from "@/components/resource-sidebar"
import { StatusBadge, StatusDot } from "@/components/status"
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle, AlertDialogTrigger } from "@/components/ui/alert-dialog"
import { Button } from "@/components/ui/button"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { Textarea } from "@/components/ui/textarea"
import { WorkspaceHeader } from "@/components/workspace-header"
import { ReviewPanel } from "@/features/projects/review-panel"
import { SessionWorkspace } from "@/features/session/session-workspace"
import { CreateProjectDialog } from "@/features/projects/create-project-dialog"

type ProjectView = "agents" | "review" | "repository" | "skills"

export default function ProjectsPage() {
  const { projectId } = useParams()
  const navigate = useNavigate()
  const [search, setSearch] = useSearchParams()
  const queryClient = useQueryClient()
  const projects = useQuery(projectsQuery())
  const selectedId = projectId || projects.data?.[0]?.id || ""
  const selectedAgentId = search.get("agent")
  const selectedSessionId = search.get("session")
  const view = (search.get("view") as ProjectView | null) || "agents"
  const detail = useQuery(projectQuery(selectedId, selectedAgentId, selectedSessionId))
  const [createOpen, setCreateOpen] = useState(false)

  useEffect(() => {
    if (!projectId && selectedId) navigate(`/projects/${selectedId}`, { replace: true })
  }, [navigate, projectId, selectedId])

  if (projects.isLoading) return <LoadingState rows={6} />
  if (projects.error) return <ErrorState error={projects.error} retry={() => void projects.refetch()} />

  const items = (projects.data ?? []).map((project) => ({
    id: project.id,
    title: project.name,
    subtitle: String(project.repository_full_name || project.repository || "Repository"),
    status: <StatusDot status={project.status} />,
    icon: project.name.slice(0, 1).toUpperCase(),
  }))
  const changeSearch = (patch: Record<string, string | null>) => {
    const next = new URLSearchParams(search)
    for (const [key, value] of Object.entries(patch)) {
      if (value) next.set(key, value)
      else next.delete(key)
    }
    setSearch(next)
  }

  return (
    <div className="relative flex h-full min-h-0">
      <ResourceSidebar title="Projects" items={items} selectedId={selectedId} onSelect={(id) => navigate(`/projects/${id}`)} onCreate={() => setCreateOpen(true)} />
      {!selectedId
        ? <ProjectPageState><EmptyState title="No projects yet" description="Connect a repository to create a maintainer and review workspace." action={<Button onClick={() => setCreateOpen(true)}><Plus data-icon="inline-start" /> New project</Button>} /></ProjectPageState>
        : detail.isLoading
          ? <ProjectPageState><LoadingState rows={7} /></ProjectPageState>
          : detail.error
            ? <ProjectPageState><ErrorState error={detail.error} retry={() => void detail.refetch()} /></ProjectPageState>
            : detail.data
              ? <ProjectWorkspace detail={detail.data} view={view} setView={(next) => changeSearch({ view: next })} selectedSessionId={selectedSessionId} selectAgent={(id) => changeSearch({ agent: id, session: null })} selectSession={(id) => changeSearch({ session: id })} refresh={() => queryClient.invalidateQueries({ queryKey: ["projects", selectedId] })} onDeleted={async () => { await queryClient.invalidateQueries({ queryKey: queryKeys.projects }); navigate("/projects") }} />
              : null}
      <CreateProjectDialog open={createOpen} onOpenChange={setCreateOpen} onCreated={(project) => {
        setCreateOpen(false)
        void queryClient.invalidateQueries({ queryKey: queryKeys.projects })
        navigate(`/projects/${project.id}`)
      }} />
    </div>
  )
}

function ProjectPageState({ children }: { children: React.ReactNode }) {
  return <section className="flex min-w-0 flex-1 flex-col"><WorkspaceHeader crumbs={[{ label: "Projects" }]} /><div className="min-h-0 flex-1 overflow-auto">{children}</div></section>
}

function ProjectWorkspace({ detail, view, setView, selectedSessionId, selectAgent, selectSession, refresh, onDeleted }: {
  detail: ProjectDetail
  view: ProjectView
  setView(view: ProjectView): void
  selectedSessionId: string | null
  selectAgent(id: string): void
  selectSession(id: string): void
  refresh(): Promise<unknown>
  onDeleted(): Promise<void>
}) {
  const selected = detail.selected_agent || detail.maintainer_agent
  return (
    <section className="flex h-full min-h-0 min-w-0 flex-1 flex-col bg-background">
      <WorkspaceHeader
        crumbs={[{ label: "Projects", href: "/projects" }, { label: detail.name }, { label: view.slice(0, 1).toUpperCase() + view.slice(1) }]}
        actions={<><StatusBadge status={detail.status} /><ProjectAutoReview project={detail} refresh={refresh} /></>}
      />
      <div className="flex shrink-0 items-center border-b px-4 md:px-6">
        <Tabs value={view} onValueChange={(value: string) => setView(value as ProjectView)}>
          <TabsList variant="line" className="h-11">
            <TabsTrigger value="agents"><Users /> Agents</TabsTrigger>
            <TabsTrigger value="review"><ShieldCheck /> Review</TabsTrigger>
            <TabsTrigger value="repository"><GitBranch /> Repository</TabsTrigger>
            <TabsTrigger value="skills"><Search /> Skills</TabsTrigger>
          </TabsList>
        </Tabs>
      </div>
      {view === "agents" && <div className="flex min-h-0 flex-1 flex-col">
        <AgentStrip agents={detail.agents} selectedId={selected.id} onSelect={selectAgent} />
        <SessionWorkspace
          agent={selected}
          sessionId={selectedSessionId || selected.selected_session_id}
          skillsEndpoint={`/projects/${detail.id}/skills`}
          onSelectSession={selectSession}
          onAgentUpdated={refresh}
          onCreateSession={async () => {
            const response = await api<{ session?: { id: string }; id?: string }>(`/agents/${selected.id}/sessions`, { method: "POST" })
            const id = response.session?.id || response.id
            if (id) selectSession(id)
            await refresh()
          }}
          onSend={async (message, skillMentions) => {
            await api(`/projects/${detail.id}/messages`, { method: "POST", ...jsonBody({ message, skill_mentions: skillMentions, session_id: selectedSessionId || selected.selected_session_id }) })
          }}
          onStop={async (turnId) => {
            await api(`/agents/${selected.id}/turns/${turnId}/cancel`, { method: "POST" })
          }}
        />
      </div>}
      {view === "review" && <ReviewPanel project={detail} />}
      {view === "repository" && <RepositoryPanel project={detail} refresh={refresh} onDeleted={onDeleted} />}
      {view === "skills" && <SkillsPanel projectId={detail.id} />}
    </section>
  )
}

function AgentStrip({ agents, selectedId, onSelect }: { agents: ProjectDetail["agents"]; selectedId: string; onSelect(id: string): void }) {
  return (
    <div className="shrink-0 overflow-x-auto border-b bg-muted/20 p-2"><Tabs value={selectedId} onValueChange={onSelect}><TabsList>{agents.map((agent) => <TabsTrigger key={agent.id} value={agent.id} className="min-w-52 justify-start"><Bot data-icon="inline-start" /><span className="min-w-0 flex-1 text-left"><span className="block truncate text-xs font-medium">{agent.name}</span><span className="block truncate text-[11px] text-muted-foreground">{agent.role} · {agent.model}</span></span><StatusDot status={agent.state.runtime?.activity || agent.state.resource} /></TabsTrigger>)}</TabsList></Tabs></div>
  )
}

function ProjectAutoReview({ project, refresh }: { project: ProjectDetail; refresh(): Promise<unknown> }) {
  const enabled = Boolean(project.auto_review_enabled)
  return (
    <Field orientation="horizontal" className="w-auto">
      <FieldLabel htmlFor="auto-review" className="text-xs font-normal">Auto review</FieldLabel>
      <Switch checked={enabled} onCheckedChange={async (checked: boolean) => {
        try {
          await api(`/projects/${project.id}`, { method: "PATCH", ...jsonBody({ auto_review_enabled: checked }) })
          await refresh()
        } catch (error) { toast.error(error instanceof Error ? error.message : String(error)) }
      }} id="auto-review" />
    </Field>
  )
}

function RepositoryPanel({ project, refresh, onDeleted }: { project: ProjectDetail; refresh(): Promise<unknown>; onDeleted(): Promise<void> }) {
  const [name, setName] = useState(project.name)
  const [image, setImage] = useState(String(project.docker_image || ""))
  const [reviewerPrompt, setReviewerPrompt] = useState(String(project.reviewer_extra_prompt || ""))
  const [saving, setSaving] = useState(false)
  useEffect(() => { setName(project.name); setImage(String(project.docker_image || "")); setReviewerPrompt(String(project.reviewer_extra_prompt || "")) }, [project])
  const save = async () => {
    setSaving(true)
    try {
      await api(`/projects/${project.id}`, { method: "PATCH", ...jsonBody({ name: name.trim() || project.name, docker_image: image.trim() || null, reviewer_extra_prompt: reviewerPrompt.trim() || null }) })
      await refresh()
      toast.success("Project settings saved")
    } catch (error) { toast.error(error instanceof Error ? error.message : String(error)) } finally { setSaving(false) }
  }
  const remove = async () => {
    try { await api(`/projects/${project.id}`, { method: "DELETE" }); await onDeleted(); toast.success("Project deleted") } catch (error) { toast.error(error instanceof Error ? error.message : String(error)) }
  }
  return (
    <div className="min-h-0 flex-1 overflow-auto p-5 md:p-7"><div className="mx-auto flex max-w-3xl flex-col gap-5">
      <div className="divide-y rounded-lg border"><InfoRow label="Repository" value={String(project.repository_full_name || project.repository || "—")} /><InfoRow label="Default branch" value={String(project.branch || project.default_branch || "—")} /><InfoRow label="Clone status" value={String(project.clone_status || project.status)} /><InfoRow label="Authentication" value={project.auth_status || "—"} /></div>
      <section className="flex flex-col gap-4 rounded-lg border p-5"><div><h2 className="text-sm font-semibold">Project settings</h2><p className="text-xs text-muted-foreground">Repository workspace and reviewer behavior.</p></div><FieldGroup><Field><FieldLabel htmlFor="project-name">Name</FieldLabel><Input id="project-name" value={name} onChange={(event) => setName(event.target.value)} /></Field><Field><FieldLabel htmlFor="project-image">Docker image</FieldLabel><Input id="project-image" value={image} onChange={(event) => setImage(event.target.value)} placeholder="Server default" /></Field><Field><FieldLabel htmlFor="reviewer-instructions">Reviewer instructions</FieldLabel><Textarea id="reviewer-instructions" className="min-h-24" value={reviewerPrompt} onChange={(event) => setReviewerPrompt(event.target.value)} /></Field></FieldGroup><div className="flex justify-between"><AlertDialog><AlertDialogTrigger asChild><Button variant="destructive"><Trash2 data-icon="inline-start" /> Delete project</Button></AlertDialogTrigger><AlertDialogContent><AlertDialogHeader><AlertDialogTitle>Delete {project.name}?</AlertDialogTitle><AlertDialogDescription>This removes project agents and local workspaces. The remote repository is not changed.</AlertDialogDescription></AlertDialogHeader><AlertDialogFooter><AlertDialogCancel>Cancel</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={() => void remove()}>Delete project</AlertDialogAction></AlertDialogFooter></AlertDialogContent></AlertDialog><Button disabled={saving} onClick={() => void save()}><Save data-icon="inline-start" /> Save changes</Button></div></section>
    </div></div>
  )
}

function InfoRow({ label, value }: { label: string; value: string }) {
  return <div className="grid gap-1 px-4 py-3 sm:grid-cols-[10rem_1fr]"><span className="text-xs font-medium text-muted-foreground">{label}</span><span className="break-all text-sm">{value}</span></div>
}

function SkillsPanel({ projectId }: { projectId: string }) {
  const query = useQuery({ queryKey: ["projects", projectId, "skills"], queryFn: () => api<{ skills?: Array<{ name: string; description?: string; path?: string }>; roots?: string[]; errors?: string[] }>(`/projects/${projectId}/skills`) })
  const detect = useMutation({
    mutationFn: () => api(`/projects/${projectId}/skills/detect`, { method: "POST" }),
    onSuccess: () => void query.refetch(),
    onError: (error) => toast.error(error.message),
  })
  if (query.isLoading) return <LoadingState rows={4} />
  if (query.error) return <ErrorState error={query.error} retry={() => void query.refetch()} />
  return (
    <div className="min-h-0 flex-1 overflow-auto p-5 md:p-7"><div className="mx-auto max-w-4xl">
      <div className="mb-4 flex items-center justify-between"><div><h2 className="text-sm font-semibold">Project skills</h2><p className="text-xs text-muted-foreground">Detected from the canonical default-branch repository.</p></div><Button variant="outline" onClick={() => detect.mutate()}><RefreshCw data-icon="inline-start" /> Detect</Button></div>
      {(query.data?.skills ?? []).length === 0 ? <EmptyState title="No project skills found" description="Detect skills after the default branch repository is ready." /> : <div className="divide-y rounded-lg border">{(query.data?.skills ?? []).map((skill) => <div key={skill.name} className="px-4 py-3"><div className="text-sm font-medium">{skill.name}</div><div className="mt-0.5 text-xs text-muted-foreground">{skill.description || skill.path}</div></div>)}</div>}
    </div></div>
  )
}
