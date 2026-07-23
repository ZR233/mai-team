import { CircleStop, MessageSquarePlus, Send, Sparkles } from "lucide-react"
import { useEffect, useRef, useState } from "react"

import type { AgentDetail, SessionSummary } from "@/api/product-types"
import { ConnectionStatus, StatusBadge } from "@/components/status"
import { Button } from "@/components/ui/button"
import { InputGroup, InputGroupAddon, InputGroupTextarea } from "@/components/ui/input-group"
import { Progress } from "@/components/ui/progress"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { WorkspaceHeader, type WorkspaceCrumb } from "@/components/workspace-header"
import type { SessionViewSnapshot } from "@/events/session-events.generated"
import { useSessionEvents } from "@/events/use-session-events"
import { useSessionStore } from "@/events/session-store"
import { AgentModelDialog } from "@/features/agents/agent-model-dialog"
import { SessionTimeline } from "@/features/session/session-timeline"
import { SessionTodoDrawer, SessionTodoRail, latestTodoSnapshot } from "@/features/session/session-todo-panel"
import { SkillMentionPicker } from "@/features/session/skill-mention-picker"
import { cn } from "@/lib/utils"

interface SessionWorkspaceProps {
  agent: AgentDetail
  sessionId?: string | null
  onSelectSession(id: string): void
  onCreateSession(): Promise<unknown>
  onSend(message: string, skillMentions: string[]): Promise<unknown>
  onStop?(turnId: string): Promise<unknown>
  onAgentUpdated?(): Promise<unknown>
  headerActions?: React.ReactNode
  showSessions?: boolean
  skillsEndpoint?: string
  workspaceCrumbs?: WorkspaceCrumb[]
}

export function SessionWorkspace({
  agent,
  sessionId,
  onSelectSession,
  onCreateSession,
  onSend,
  onStop,
  onAgentUpdated,
  headerActions,
  showSessions = true,
  skillsEndpoint = "/skills",
  workspaceCrumbs,
}: SessionWorkspaceProps) {
  const effectiveSessionId = sessionId || agent.selected_session_id
  useSessionEvents(effectiveSessionId)
  const view = useSessionStore((state) => state.view)
  const connection = useSessionStore((state) => state.connection)
  const connectionMessage = useSessionStore((state) => state.connectionMessage)
  const [draft, setDraft] = useState("")
  const [sending, setSending] = useState(false)
  const [selectedSkills, setSelectedSkills] = useState<string[]>([])
  const [todoExpanded, setTodoExpanded] = useState(true)
  const scroller = useRef<HTMLDivElement>(null)
  const canonicalTurn = view.sessionId === effectiveSessionId ? view.turn : undefined
  const activeTurnId = activeSessionTurnId(canonicalTurn)
  const todoSnapshot = view.sessionId === effectiveSessionId ? latestTodoSnapshot(view.timelineEvents) : null

  useEffect(() => {
    const viewport = scroller.current?.querySelector("[data-radix-scroll-area-viewport]") as HTMLElement | null
    if (viewport) viewport.scrollTop = viewport.scrollHeight
  }, [view.messageOrder.length, view.parts])

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

  return (
    <section className="@container/session-workspace flex h-full min-h-0 flex-1 flex-col bg-background">
      {workspaceCrumbs ? (
        <WorkspaceHeader
          crumbs={workspaceCrumbs}
          actions={<><StatusBadge status={canonicalTurn?.status || agent.state.resource} />{onAgentUpdated && <AgentModelDialog agent={agent} onSaved={onAgentUpdated} />}{headerActions}</>}
        />
      ) : <header className="flex min-h-14 shrink-0 items-center gap-3 border-b px-4 md:px-6">
        <div className="flex size-8 shrink-0 items-center justify-center rounded-lg bg-primary font-semibold text-primary-foreground">{agent.name.slice(0, 1).toUpperCase()}</div>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2"><h2 className="truncate text-sm font-semibold">{agent.name}</h2><StatusBadge status={canonicalTurn?.status || agent.state.resource} /></div>
          <p className="truncate text-xs text-muted-foreground">{agent.role || "agent"} · {agent.provider_name} / {agent.model}</p>
        </div>
        <div className="flex items-center gap-1">
          {onAgentUpdated && <AgentModelDialog agent={agent} onSaved={onAgentUpdated} />}
          {headerActions}
        </div>
      </header>}

      {showSessions && <SessionTabs sessions={agent.sessions} selectedId={effectiveSessionId} onSelect={onSelectSession} onCreate={onCreateSession} />}

      <ScrollArea ref={scroller} className="min-h-0 flex-1" data-session-timeline-scroll>
        <div className={cn(
          "mx-auto grid w-full max-w-5xl px-5 md:px-8",
          todoSnapshot && "pb-20 transition-[grid-template-columns] duration-200 motion-reduce:transition-none @min-[52rem]/session-workspace:max-w-7xl @min-[52rem]/session-workspace:gap-6 @min-[52rem]/session-workspace:pb-0",
          todoSnapshot && (todoExpanded
            ? "@min-[52rem]/session-workspace:grid-cols-[minmax(0,1fr)_17rem]"
            : "@min-[52rem]/session-workspace:grid-cols-[minmax(0,1fr)_3rem]"),
        )}>
          <SessionTimeline view={view} />
          {todoSnapshot && (
            <SessionTodoRail
              snapshot={todoSnapshot}
              expanded={todoExpanded}
              onExpandedChange={setTodoExpanded}
            />
          )}
        </div>
      </ScrollArea>

      <div className="relative shrink-0 border-t bg-background px-3 py-3 md:px-6">
        {todoSnapshot && <SessionTodoDrawer key={effectiveSessionId} snapshot={todoSnapshot} />}
        <div className="mx-auto max-w-5xl">
          <div className="mb-2 flex flex-wrap items-center justify-between gap-2 text-xs text-muted-foreground">
            <ConnectionStatus status={connection} message={connectionMessage} />
            <div className="flex flex-wrap items-center gap-4">
              <span>Model <strong className="font-medium text-foreground">{view.runtime?.usage.model || agent.model}</strong></span>
              <span>Tokens <strong className="font-medium text-foreground">{(view.runtime?.usage.totalTokens ?? agent.token_usage.total_tokens).toLocaleString()}</strong></span>
              <span className="flex items-center gap-2">Context <strong className="font-medium text-foreground">{contextLabel(view.runtime?.usage.latestContextTokens, view.runtime?.usage.contextWindow)}</strong><Progress className="w-16" value={contextPercent(view.runtime?.usage.latestContextTokens, view.runtime?.usage.contextWindow)} /></span>
            </div>
          </div>
          <InputGroup className="h-auto flex-col items-stretch">
            <InputGroupAddon align="block-start" className="justify-start">
              <SkillMentionPicker endpoint={skillsEndpoint} selected={selectedSkills} onChange={setSelectedSkills} />
            </InputGroupAddon>
            <InputGroupTextarea
              value={draft}
              onChange={(event) => setDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && !event.shiftKey) {
                  event.preventDefault()
                  void submit()
                }
              }}
              placeholder="Send a command or message…"
              className="max-h-40 min-h-16"
            />
            <InputGroupAddon align="block-end" className="justify-between border-t">
              <span className="hidden text-xs text-muted-foreground sm:inline">Enter to send · Shift+Enter for a new line</span>
              <span className="ml-auto flex items-center gap-2">
              {activeTurnId && onStop
                ? <Button variant="outline" className="shrink-0 text-destructive" onClick={() => void onStop(activeTurnId)}><CircleStop data-icon="inline-start" /> Stop</Button>
                : <Button className="shrink-0" disabled={!draft.trim() || sending} onClick={() => void submit()}>{sending ? <Sparkles data-icon="inline-start" className="animate-pulse" /> : <Send data-icon="inline-start" />} Send</Button>}
              </span>
            </InputGroupAddon>
          </InputGroup>
        </div>
      </div>
    </section>
  )
}

const ACTIVE_TURN_STATUSES = new Set([
  "queued",
  "contextLoading",
  "waitingForModel",
  "streaming",
  "waitingForInteraction",
  "runningTool",
  "persisting",
])

export function activeSessionTurnId(turn: SessionViewSnapshot["turn"]): string | null {
  return turn && ACTIVE_TURN_STATUSES.has(turn.status) ? turn.turnId : null
}

function SessionTabs({ sessions, selectedId, onSelect, onCreate }: { sessions: SessionSummary[]; selectedId?: string | null; onSelect(id: string): void; onCreate(): Promise<unknown> }) {
  return (
    <div className="flex h-11 shrink-0 items-center gap-1 overflow-x-auto border-b px-4 md:px-6">
      <Tabs value={selectedId || ""} onValueChange={onSelect}>
        <TabsList variant="line">
          {sessions.map((session, index) => <TabsTrigger key={session.id} value={session.id}>{session.title || `Chat ${index + 1}`}</TabsTrigger>)}
        </TabsList>
      </Tabs>
      <Button variant="ghost" size="icon-sm" onClick={() => void onCreate()} aria-label="New session"><MessageSquarePlus data-icon="inline-start" /></Button>
    </div>
  )
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
