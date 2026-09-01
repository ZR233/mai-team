import { CircleStop, Send, Sparkles } from "lucide-react"
import { useEffect, useRef, useState } from "react"

import type { AgentDetail } from "@/api/product-types"
import { ConnectionStatus, StatusBadge } from "@/components/status"
import { Button } from "@/components/ui/button"
import { InputGroup, InputGroupAddon, InputGroupTextarea } from "@/components/ui/input-group"
import { Progress } from "@/components/ui/progress"
import { ScrollArea } from "@/components/ui/scroll-area"
import { WorkspaceHeader, type WorkspaceCrumb } from "@/components/workspace-header"
import { useThreadEvents } from "@/events/use-thread-events"
import { agentCanRunThread, agentPresentationStatus } from "@/features/agents/agent-lifecycle"
import { AgentModelDialog } from "@/features/agents/agent-model-dialog"
import { ActiveSkillsStatus } from "@/features/thread/active-skills-status"
import { SkillMentionPicker } from "@/features/thread/skill-mention-picker"
import { ThreadTimeline } from "@/features/thread/timeline"

const NO_ACTIVE_SKILLS: readonly string[] = []

interface ThreadWorkspaceProps {
  agent: AgentDetail
  onSend(message: string, skillMentions: string[]): Promise<unknown>
  onStop?(turnId: string): Promise<unknown>
  onAgentUpdated?(): Promise<unknown>
  headerActions?: React.ReactNode
  skillsEndpoint?: string
  workspaceCrumbs?: WorkspaceCrumb[]
}

export function ThreadWorkspace({ agent, onSend, onStop, onAgentUpdated, headerActions, skillsEndpoint = "/skills", workspaceCrumbs }: ThreadWorkspaceProps) {
  const live = useThreadEvents(agent.thread.id)
  const [draft, setDraft] = useState("")
  const [sending, setSending] = useState(false)
  const [selectedSkills, setSelectedSkills] = useState<string[]>([])
  const scroller = useRef<HTMLDivElement>(null)
  const canRun = agentCanRunThread(agent)
  const activeTurnId = live.snapshot?.activeTurn?.id ?? null
  const activeSkills = live.snapshot?.runtime?.activeSkills ?? NO_ACTIVE_SKILLS
  const presentationStatus = agentPresentationStatus(agent, live.snapshot?.thread.status)

  useEffect(() => {
    const viewport = scroller.current?.querySelector("[data-radix-scroll-area-viewport]") as HTMLElement | null
    if (viewport) viewport.scrollTop = viewport.scrollHeight
  }, [live.snapshot?.revision])

  const submit = async () => {
    const message = draft.trim()
    if (!message || sending) return
    setSending(true)
    setDraft("")
    try {
      await onSend(message, selectedSkills)
      setSelectedSkills([])
    } catch (error) {
      setDraft(message)
      throw error
    } finally {
      setSending(false)
    }
  }

  return <section className="flex h-full min-h-0 min-w-0 flex-1 flex-col bg-background">
    {workspaceCrumbs ? <WorkspaceHeader crumbs={workspaceCrumbs} actions={<><StatusBadge status={presentationStatus} />{onAgentUpdated && <AgentModelDialog agent={agent} onSaved={onAgentUpdated} />}{headerActions}</>} /> : <header className="flex min-h-14 shrink-0 items-center gap-3 border-b px-4 md:px-6"><div className="flex size-8 items-center justify-center rounded-lg bg-primary font-semibold text-primary-foreground">{agent.name.slice(0, 1).toUpperCase()}</div><div className="min-w-0 flex-1"><div className="flex items-center gap-2"><h2 className="truncate text-sm font-semibold">{agent.name}</h2><StatusBadge status={presentationStatus} /></div><p className="truncate text-xs text-muted-foreground">{agent.role || "agent"} · {agent.provider_name} / {agent.model}</p></div>{onAgentUpdated && <AgentModelDialog agent={agent} onSaved={onAgentUpdated} />}{headerActions}</header>}
    <ScrollArea ref={scroller} className="min-h-0 min-w-0 flex-1 overflow-hidden [&_[data-slot=scroll-area-viewport]>div]:!block"><div className="mx-auto w-full max-w-5xl px-5 md:px-8"><ThreadTimeline snapshot={live.snapshot} /></div></ScrollArea>
    <div className="shrink-0 border-t bg-background px-3 py-3 md:px-6"><div className="mx-auto max-w-5xl"><div className="mb-2 flex flex-wrap items-center justify-between gap-2 text-xs text-muted-foreground"><ConnectionStatus status={live.connection} message={live.connectionMessage} /><div className="flex flex-wrap items-center justify-end gap-x-4 gap-y-1"><ActiveSkillsStatus skills={activeSkills} /><span className="whitespace-nowrap">Model <strong className="font-medium text-foreground">{live.snapshot?.runtime?.usage.model || agent.model}</strong></span><span className="whitespace-nowrap">Tokens <strong className="font-medium text-foreground">{(live.snapshot?.runtime?.usage.totalTokens ?? agent.token_usage.totalTokens).toLocaleString()}</strong></span><span className="flex items-center gap-2 whitespace-nowrap">Context <strong className="font-medium text-foreground">{contextLabel(live.snapshot?.runtime?.usage.latestContextTokens, live.snapshot?.runtime?.usage.contextWindow)}</strong><Progress className="w-16" value={contextPercent(live.snapshot?.runtime?.usage.latestContextTokens, live.snapshot?.runtime?.usage.contextWindow)} /></span></div></div>
      <InputGroup className="h-auto flex-col items-stretch"><InputGroupAddon align="block-start" className="justify-start"><SkillMentionPicker endpoint={skillsEndpoint} selected={selectedSkills} onChange={setSelectedSkills} /></InputGroupAddon><InputGroupTextarea value={draft} onChange={(event) => setDraft(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter" && !event.shiftKey) { event.preventDefault(); void submit() } }} placeholder="Send a command or message…" className="max-h-40 min-h-16" /><InputGroupAddon align="block-end" className="justify-between border-t"><span className="hidden text-xs text-muted-foreground sm:inline">Enter to send · Shift+Enter for a new line</span><span className="ml-auto flex items-center gap-2">{activeTurnId && onStop ? <Button variant="outline" className="text-destructive" onClick={() => void onStop(activeTurnId)}><CircleStop data-icon="inline-start" /> Stop</Button> : <Button disabled={!canRun || !draft.trim() || sending} onClick={() => void submit()}>{sending ? <Sparkles data-icon="inline-start" className="animate-pulse" /> : <Send data-icon="inline-start" />} Send</Button>}</span></InputGroupAddon></InputGroup>
    </div></div>
  </section>
}

function contextLabel(tokens?: number, window?: number) {
  if (!tokens && !window) return "—"
  const compact = (value?: number) => value ? value >= 1000 ? `${(value / 1000).toFixed(1)}K` : String(value) : "—"
  return `${compact(tokens)} / ${compact(window)}`
}

function contextPercent(tokens?: number, window?: number) {
  if (!tokens || !window) return 0
  return Math.min(100, Math.round((tokens / window) * 100))
}
