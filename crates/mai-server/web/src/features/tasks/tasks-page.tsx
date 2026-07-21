import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { ArrowLeft, Bot, Check, ChevronDown, ClipboardList, Download, Plus, Sparkles, Trash2, X } from "lucide-react"
import { useEffect, useState } from "react"
import { Link, useNavigate, useParams, useSearchParams } from "react-router-dom"
import { toast } from "sonner"

import { api, jsonBody } from "@/api/client"
import type { TaskDetail, TaskSummary } from "@/api/product-types"
import { queryKeys, tasksQuery } from "@/api/queries"
import { Markdown } from "@/components/markdown"
import { EmptyState, ErrorState, LoadingState } from "@/components/page-state"
import { ResourceSidebar } from "@/components/resource-sidebar"
import { StatusBadge, StatusDot } from "@/components/status"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle, AlertDialogTrigger } from "@/components/ui/alert-dialog"
import { Button } from "@/components/ui/button"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { Textarea } from "@/components/ui/textarea"
import { WorkspaceHeader } from "@/components/workspace-header"
import { SessionWorkspace } from "@/features/session/session-workspace"

export default function TasksPage() {
  const { taskId } = useParams()
  const navigate = useNavigate()
  const [search, setSearch] = useSearchParams()
  const queryClient = useQueryClient()
  const tasks = useQuery(tasksQuery())
  const selectedId = taskId || tasks.data?.[0]?.id || ""
  const selectedAgentId = search.get("agent")
  const detail = useQuery({ queryKey: ["tasks", selectedId, selectedAgentId || "selected"], queryFn: () => api<TaskDetail>(`/tasks/${selectedId}${selectedAgentId ? `?agent_id=${encodeURIComponent(selectedAgentId)}` : ""}`), enabled: Boolean(selectedId) })
  const [createOpen, setCreateOpen] = useState(false)
  useEffect(() => { if (!taskId && selectedId) navigate(`/tasks/${selectedId}`, { replace: true }) }, [navigate, selectedId, taskId])
  const refresh = async () => { await Promise.all([queryClient.invalidateQueries({ queryKey: queryKeys.tasks }), selectedId ? queryClient.invalidateQueries({ queryKey: ["tasks", selectedId] }) : Promise.resolve()]) }
  if (tasks.isLoading) return <LoadingState rows={6} />
  if (tasks.error) return <ErrorState error={tasks.error} retry={() => void tasks.refetch()} />
  const items = (tasks.data ?? []).map((task) => ({ id: task.id, title: task.title, subtitle: `Plan ${task.plan_status} · ${task.agent_count} agents`, status: <StatusDot status={task.status} />, icon: <ClipboardList className="size-4" /> }))

  return <div className="relative flex h-full min-h-0">
    <ResourceSidebar
      title="Tasks"
      items={items}
      selectedId={selectedId}
      onSelect={(id) => navigate(`/tasks/${id}`)}
      onCreate={() => setCreateOpen(true)}
      footer={<div className="space-y-1">
        <Button variant="ghost" className="w-full justify-start" onClick={() => void ensureDefaultTask(queryClient, navigate)}><Sparkles data-icon="inline-start" /> Ensure default</Button>
        <Button variant="ghost" asChild className="w-full justify-start"><Link to="/chat"><ArrowLeft data-icon="inline-start" /> Back to chat</Link></Button>
      </div>}
    />
    {!selectedId ? <EmptyState title="No tasks yet" description="Create a planned multi-agent task." action={<Button onClick={() => setCreateOpen(true)}><Plus data-icon="inline-start" /> New task</Button>} /> : detail.isLoading ? <div className="min-w-0 flex-1"><LoadingState rows={7} /></div> : detail.error ? <div className="min-w-0 flex-1"><ErrorState error={detail.error} retry={() => void detail.refetch()} /></div> : detail.data ? <TaskWorkspace detail={detail.data} selectAgent={(id) => setSearch({ agent: id })} refresh={refresh} onDeleted={() => navigate("/tasks")} /> : null}
    <CreateTaskDialog open={createOpen} onOpenChange={setCreateOpen} onCreated={(task) => { setCreateOpen(false); void refresh(); navigate(`/tasks/${task.id}`) }} />
  </div>
}

async function ensureDefaultTask(queryClient: ReturnType<typeof useQueryClient>, navigate: ReturnType<typeof useNavigate>) {
  try {
    const task = await api<TaskSummary | null>("/tasks:ensure-default", { method: "POST" })
    await queryClient.invalidateQueries({ queryKey: queryKeys.tasks })
    if (task) navigate(`/tasks/${task.id}`)
  } catch (error) {
    toast.error(error instanceof Error ? error.message : String(error))
  }
}

function TaskWorkspace({ detail, selectAgent, refresh, onDeleted }: { detail: TaskDetail; selectAgent(id: string): void; refresh(): Promise<void>; onDeleted(): void }) {
  const selected = detail.selected_agent
  const [feedback, setFeedback] = useState("")
  const canApprove = detail.status === "awaiting_approval" && detail.plan.status === "ready"
  const action = async (path: string, body?: unknown) => { try { await api(`/tasks/${detail.id}/${path}`, { method: "POST", ...(body ? jsonBody(body) : {}) }); await refresh() } catch (error) { toast.error(error instanceof Error ? error.message : String(error)) } }
  const remove = async () => { try { await api(`/tasks/${detail.id}`, { method: "DELETE" }); onDeleted() } catch (error) { toast.error(error instanceof Error ? error.message : String(error)) } }

  return <section className="flex h-full min-h-0 min-w-0 flex-1 flex-col bg-background">
    <WorkspaceHeader
      crumbs={[{ label: "Tasks", href: "/tasks" }, { label: detail.title }]}
      actions={<><StatusBadge status={detail.status} /><Button variant="outline" size="sm" onClick={() => void action("cancel")}><X data-icon="inline-start" /> Cancel</Button><AlertDialog><AlertDialogTrigger asChild><Button variant="ghost" size="sm"><Trash2 data-icon="inline-start" /> Delete</Button></AlertDialogTrigger><AlertDialogContent><AlertDialogHeader><AlertDialogTitle>Delete this task?</AlertDialogTitle><AlertDialogDescription>This removes its agents, sessions, and local task state.</AlertDialogDescription></AlertDialogHeader><AlertDialogFooter><AlertDialogCancel>Cancel</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={() => void remove()}>Delete task</AlertDialogAction></AlertDialogFooter></AlertDialogContent></AlertDialog></>}
    />
    <Collapsible defaultOpen={canApprove} className="shrink-0 border-b bg-muted/20">
      <CollapsibleTrigger className="flex w-full items-center gap-2 px-5 py-3 text-left text-sm font-medium">
        <ChevronDown className="size-4" />
        {detail.plan.title || "Task plan"}
        <span className="text-xs font-normal text-muted-foreground">{detail.plan.markdown ? `v${detail.plan.version}` : "Waiting for planner"}</span>
      </CollapsibleTrigger>
      {detail.plan.markdown && <CollapsibleContent className="max-h-72 overflow-auto border-t bg-background px-5 py-4"><Markdown>{detail.plan.markdown}</Markdown>{canApprove && <Alert className="mt-4"><Check /><AlertTitle>Plan ready for approval</AlertTitle><AlertDescription><div className="mt-3 flex flex-wrap items-center gap-2"><Button onClick={() => void action("plan:approve")}><Check data-icon="inline-start" /> Approve plan</Button><Input className="min-w-56 flex-1" value={feedback} onChange={(event) => setFeedback(event.target.value)} placeholder="Revision feedback" /><Button variant="outline" disabled={!feedback.trim()} onClick={() => void action("plan:request-revision", { feedback }).then(() => setFeedback(""))}>Request revision</Button></div></AlertDescription></Alert>}</CollapsibleContent>}
    </Collapsible>
    {detail.artifacts.length > 0 && <div className="flex shrink-0 gap-2 overflow-x-auto border-b px-4 py-2">{detail.artifacts.map((artifact) => <Button key={artifact.id} asChild variant="outline" size="sm"><a href={`/artifacts/${encodeURIComponent(artifact.id)}/download`} download><Download data-icon="inline-start" />{artifact.name}<span className="text-muted-foreground">{formatBytes(artifact.size_bytes)}</span></a></Button>)}</div>}
    <div className="shrink-0 overflow-x-auto border-b bg-muted/20 p-2"><Tabs value={selected.id} onValueChange={selectAgent}><TabsList>{detail.agents.map((agent) => <TabsTrigger key={agent.id} value={agent.id} className="min-w-48 justify-start"><Bot data-icon="inline-start" /><span className="min-w-0 flex-1 text-left"><strong className="block truncate text-xs">{agent.name}</strong><small className="block truncate text-[11px] text-muted-foreground">{agent.role} · {agent.model}</small></span><StatusDot status={agent.state.runtime?.activity || agent.state.resource} /></TabsTrigger>)}</TabsList></Tabs></div>
    <SessionWorkspace agent={selected} sessionId={selected.selected_session_id} showSessions={false} onSelectSession={() => {}} onCreateSession={async () => {}} onAgentUpdated={refresh} onSend={(message, skillMentions) => action("messages", { message, skill_mentions: skillMentions })} onStop={async (turnId) => { await api(`/agents/${selected.id}/turns/${turnId}/cancel`, { method: "POST" }) }} />
  </section>
}

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}

function CreateTaskDialog({ open, onOpenChange, onCreated }: { open: boolean; onOpenChange(open: boolean): void; onCreated(task: TaskSummary): void }) {
  const [title, setTitle] = useState("")
  const [message, setMessage] = useState("")
  const [image, setImage] = useState("")
  const mutation = useMutation({ mutationFn: () => api<{ task: TaskSummary }>("/tasks", { method: "POST", ...jsonBody({ title: title || null, message: message || null, docker_image: image || null }) }), onSuccess: ({ task }) => { toast.success("Task created"); onCreated(task) }, onError: (error) => toast.error(error.message) })
  return <Dialog open={open} onOpenChange={onOpenChange}><DialogContent><DialogHeader><DialogTitle>New task</DialogTitle><DialogDescription>Create a planner-led task and approve its plan before execution.</DialogDescription></DialogHeader><FieldGroup><Field><FieldLabel htmlFor="task-title">Title</FieldLabel><Input id="task-title" value={title} onChange={(event) => setTitle(event.target.value)} /></Field><Field><FieldLabel htmlFor="task-request">Initial request</FieldLabel><Textarea id="task-request" value={message} onChange={(event) => setMessage(event.target.value)} /></Field><Field><FieldLabel htmlFor="task-image">Docker image</FieldLabel><Input id="task-image" value={image} onChange={(event) => setImage(event.target.value)} placeholder="Use server default" /></Field></FieldGroup><DialogFooter><Button variant="outline" onClick={() => onOpenChange(false)}>Cancel</Button><Button disabled={!message.trim() || mutation.isPending} onClick={() => mutation.mutate()}>Create task</Button></DialogFooter></DialogContent></Dialog>
}
