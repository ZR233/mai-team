import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { Boxes, ClipboardList, Plus } from "lucide-react"
import { useEffect, useState } from "react"
import { Link, useNavigate, useParams } from "react-router-dom"
import { toast } from "sonner"

import { api, jsonBody } from "@/api/client"
import type { EnvironmentDetail, EnvironmentSummary } from "@/api/product-types"
import { environmentQuery, environmentsQuery, queryKeys } from "@/api/queries"
import { EmptyState, ErrorState, LoadingState } from "@/components/page-state"
import { ResourceSidebar } from "@/components/resource-sidebar"
import { StatusDot } from "@/components/status"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { WorkspaceHeader } from "@/components/workspace-header"
import { ThreadWorkspace } from "@/features/thread/thread-workspace"

export default function ChatPage() {
  const { environmentId } = useParams()
  const navigate = useNavigate()
  const queryClient = useQueryClient()
  const environments = useQuery(environmentsQuery())
  const selectedId = environmentId || environments.data?.[0]?.id || ""
  const detail = useQuery(environmentQuery(selectedId))
  const [createOpen, setCreateOpen] = useState(false)

  useEffect(() => {
    if (!environmentId && selectedId) navigate(`/chat/${selectedId}`, { replace: true })
  }, [environmentId, navigate, selectedId])

  const refresh = async () => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: queryKeys.environments }),
      selectedId ? queryClient.invalidateQueries({ queryKey: ["environments", selectedId] }) : Promise.resolve(),
    ])
  }

  if (environments.isLoading) return <LoadingState rows={6} />
  if (environments.error) return <ErrorState error={environments.error} retry={() => void environments.refetch()} />

  const items = (environments.data ?? []).map((environment) => ({
    id: environment.id,
    title: environment.name || environment.title || "Environment",
    subtitle: environment.root_agent?.model || "Docker workspace",
    status: <StatusDot status={environment.status || environment.root_agent?.state?.resource} />,
    icon: <Boxes className="size-4" />,
  }))

  return (
    <div className="relative flex h-full min-h-0">
      <ResourceSidebar
        title="Chat"
        items={items}
        selectedId={selectedId}
        onSelect={(id) => navigate(`/chat/${id}`)}
        onCreate={() => setCreateOpen(true)}
        footer={<div className="space-y-1"><Button variant="ghost" className="w-full justify-start" asChild><Link to="/tasks"><ClipboardList data-icon="inline-start" /> Planned tasks</Link></Button><Button variant="ghost" className="w-full justify-start" onClick={() => void ensureDefault(queryClient, navigate)}><Plus data-icon="inline-start" /> Ensure default</Button></div>}
      />
      {!selectedId
        ? <PageState><EmptyState title="No environments yet" description="Create a Docker-backed environment to start a conversation." action={<Button onClick={() => setCreateOpen(true)}><Plus data-icon="inline-start" /> New environment</Button>} /></PageState>
        : detail.isLoading
          ? <PageState><LoadingState rows={6} /></PageState>
          : detail.error
            ? <PageState><ErrorState error={detail.error} retry={() => void detail.refetch()} /></PageState>
            : detail.data
              ? <EnvironmentWorkspace detail={detail.data} refresh={refresh} />
              : null}
      <CreateEnvironmentDialog open={createOpen} onOpenChange={setCreateOpen} onCreated={(environment) => {
        setCreateOpen(false)
        void queryClient.invalidateQueries({ queryKey: queryKeys.environments })
        navigate(`/chat/${environment.id}`)
      }} />
    </div>
  )
}

function EnvironmentWorkspace({ detail, refresh }: {
  detail: EnvironmentDetail
  refresh(): Promise<void>
}) {
  const root = detail.root_agent
  const environmentName = detail.name || detail.title || root.name
  return (
    <ThreadWorkspace
      agent={root}
      workspaceCrumbs={[{ label: "Chat", href: "/chat" }, { label: environmentName }, { label: root.thread.title || "Thread" }]}
      onAgentUpdated={refresh}
      onSend={async (message, skillMentions) => {
        await api(`/threads/${encodeURIComponent(root.thread.id)}/messages`, {
          method: "POST",
          ...jsonBody({ message, skill_mentions: skillMentions }),
        })
      }}
      onStop={async (turnId) => {
        await api(`/agents/${root.id}/turns/${turnId}/cancel`, { method: "POST" })
      }}
    />
  )
}

function CreateEnvironmentDialog({ open, onOpenChange, onCreated }: { open: boolean; onOpenChange(open: boolean): void; onCreated(environment: EnvironmentSummary): void }) {
  const [name, setName] = useState("")
  const [image, setImage] = useState("")
  const mutation = useMutation({
    mutationFn: () => api<{ environment: EnvironmentSummary } | EnvironmentSummary>("/environments", { method: "POST", ...jsonBody({ name: name || null, docker_image: image || null }) }),
    onSuccess(response) {
      const environment = (response as { environment?: EnvironmentSummary }).environment ?? response as EnvironmentSummary
      toast.success("Environment created")
      onCreated(environment)
    },
    onError: (error) => toast.error(error.message),
  })
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader><DialogTitle>New environment</DialogTitle><DialogDescription>Create an isolated Docker workspace and its first conversation.</DialogDescription></DialogHeader>
        <FieldGroup>
          <Field><FieldLabel htmlFor="environment-name">Name</FieldLabel><Input id="environment-name" value={name} onChange={(event) => setName(event.target.value)} placeholder="Research workspace" /></Field>
          <Field><FieldLabel htmlFor="environment-image">Docker image</FieldLabel><Input id="environment-image" value={image} onChange={(event) => setImage(event.target.value)} placeholder="Use server default" /></Field>
        </FieldGroup>
        <DialogFooter><Button variant="outline" onClick={() => onOpenChange(false)}>Cancel</Button><Button disabled={mutation.isPending} onClick={() => mutation.mutate()}>Create</Button></DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function PageState({ children }: { children: React.ReactNode }) {
  return <section className="flex min-w-0 flex-1 flex-col"><WorkspaceHeader crumbs={[{ label: "Chat" }]} /><div className="min-h-0 flex-1 overflow-auto">{children}</div></section>
}

async function ensureDefault(queryClient: ReturnType<typeof useQueryClient>, navigate: ReturnType<typeof useNavigate>) {
  try {
    const environment = await api<EnvironmentSummary>("/environments:ensure-default", { method: "POST" })
    await queryClient.invalidateQueries({ queryKey: queryKeys.environments })
    navigate(`/chat/${environment.id}`)
  } catch (error) {
    toast.error(error instanceof Error ? error.message : String(error))
  }
}
