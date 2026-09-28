import { useInfiniteQuery, useQuery } from "@tanstack/react-query"
import { CircleStop, Send, Sparkles } from "lucide-react"
import { useEffect, useMemo, useRef, useState } from "react"

import type { AgentDetail } from "@/api/product-types"
import { activeThreadTurnQuery, threadTurnsQuery } from "@/api/queries"
import { ErrorState, LoadingState } from "@/components/page-state"
import { ConnectionStatus, StatusBadge } from "@/components/status"
import { Button } from "@/components/ui/button"
import { InputGroup, InputGroupAddon, InputGroupTextarea } from "@/components/ui/input-group"
import { Progress } from "@/components/ui/progress"
import { ScrollArea } from "@/components/ui/scroll-area"
import { WorkspaceHeader, type WorkspaceCrumb } from "@/components/workspace-header"
import type { ThreadItem, ThreadTurnHistory, ThreadTurnPage } from "@/events/thread-events.generated"
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
  // 产品 Thread 的身份就是 Agent 身份：`/threads/{id}/*` 以 agent id 作为 thread id。
  const threadId = agent.id
  const live = useThreadEvents(threadId)
  const history = useInfiniteQuery(threadTurnsQuery(threadId))
  const activeTurnId = live.snapshot?.activeTurn?.id ?? null
  const activeHistory = useQuery(activeThreadTurnQuery(threadId, activeTurnId))
  const [draft, setDraft] = useState("")
  const [sending, setSending] = useState(false)
  const [selectedSkills, setSelectedSkills] = useState<string[]>([])
  const scroller = useRef<HTMLDivElement>(null)
  const canRun = agentCanRunThread(agent)
  const activeSkills = live.snapshot?.runtime?.activeSkills ?? NO_ACTIVE_SKILLS
  const presentationStatus = agentPresentationStatus(agent, live.snapshot?.thread.status)
  const items = useMemo(
    () => composeTimelineItems(history.data?.pages, activeHistory.data?.turn.id === activeTurnId ? activeHistory.data : null),
    [history.data, activeHistory.data, activeTurnId],
  )
  const totalTokens = live.snapshot?.runtime?.usage.totalTokens ?? agent.usage.totalTokens

  // 切换 Thread 或出现新的已完成 Turn 时回到最新位置；加载更早的历史不应打断阅读。
  const headTurnId = history.data?.pages[0]?.turns[0]?.turn.id ?? ""
  const scrollAnchor = `${threadId}:${live.snapshot?.revision ?? 0}:${headTurnId}`
  const lastScrollAnchor = useRef<string | null>(null)
  useEffect(() => {
    if (lastScrollAnchor.current === scrollAnchor) return
    lastScrollAnchor.current = scrollAnchor
    const viewport = scroller.current?.querySelector("[data-radix-scroll-area-viewport]") as HTMLElement | null
    if (viewport) viewport.scrollTop = viewport.scrollHeight
  }, [scrollAnchor])

  // 活动 Turn 结束后刷新终态分页；运行中条目则来自 `/active-turn`。
  const previousActiveTurn = useRef<{ threadId: string; turnId: string | null }>({ threadId, turnId: activeTurnId })
  const refetchHistory = useRef(history.refetch)
  refetchHistory.current = history.refetch
  useEffect(() => {
    const previous = previousActiveTurn.current
    previousActiveTurn.current = { threadId, turnId: activeTurnId }
    if (previous.threadId !== threadId) return
    if (previous.turnId && !activeTurnId) void refetchHistory.current()
  }, [threadId, activeTurnId])

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
    <ScrollArea ref={scroller} className="min-h-0 min-w-0 flex-1 overflow-hidden [&_[data-slot=scroll-area-viewport]>div]:!block"><div className="mx-auto w-full max-w-5xl px-5 md:px-8">{history.isPending ? <LoadingState rows={4} /> : history.isError ? <ErrorState error={history.error} retry={() => void history.refetch()} /> : <>{activeTurnId && activeHistory.isError && <ErrorState error={activeHistory.error} retry={() => void activeHistory.refetch()} />}{history.hasNextPage && <div className="flex justify-center pt-5"><Button variant="outline" size="sm" disabled={history.isFetchingNextPage} onClick={() => void history.fetchNextPage()}>{history.isFetchingNextPage ? "Loading earlier history…" : "Load earlier history"}</Button></div>}<ThreadTimeline snapshot={live.snapshot} items={items} /></>}</div></ScrollArea>
    <div className="shrink-0 border-t bg-background px-3 py-3 md:px-6"><div className="mx-auto max-w-5xl"><div className="mb-2 flex flex-wrap items-center justify-between gap-2 text-xs text-muted-foreground"><ConnectionStatus status={live.connection} message={live.connectionMessage} /><div className="flex flex-wrap items-center justify-end gap-x-4 gap-y-1"><ActiveSkillsStatus skills={activeSkills} /><span className="whitespace-nowrap">Model <strong className="font-medium text-foreground">{live.snapshot?.runtime?.usage.model || agent.model}</strong></span><span className="whitespace-nowrap">Tokens <strong className="font-medium text-foreground">{totalTokens.toLocaleString()}</strong></span><span className="flex items-center gap-2 whitespace-nowrap">Context <strong className="font-medium text-foreground">{contextLabel(live.snapshot?.runtime?.usage.latestContextTokens, live.snapshot?.runtime?.usage.contextWindow)}</strong><Progress className="w-16" value={contextPercent(live.snapshot?.runtime?.usage.latestContextTokens, live.snapshot?.runtime?.usage.contextWindow)} /></span></div></div>
      <InputGroup className="h-auto flex-col items-stretch"><InputGroupAddon align="block-start" className="justify-start"><SkillMentionPicker endpoint={skillsEndpoint} selected={selectedSkills} onChange={setSelectedSkills} /></InputGroupAddon><InputGroupTextarea value={draft} onChange={(event) => setDraft(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter" && !event.shiftKey) { event.preventDefault(); void submit() } }} placeholder="Send a command or message…" className="max-h-40 min-h-16" /><InputGroupAddon align="block-end" className="justify-between border-t"><span className="hidden text-xs text-muted-foreground sm:inline">Enter to send · Shift+Enter for a new line</span><span className="ml-auto flex items-center gap-2">{activeTurnId && onStop ? <Button variant="outline" className="text-destructive" onClick={() => void onStop(activeTurnId)}><CircleStop data-icon="inline-start" /> Stop</Button> : <Button disabled={!canRun || !draft.trim() || sending} onClick={() => void submit()}>{sending ? <Sparkles data-icon="inline-start" className="animate-pulse" /> : <Send data-icon="inline-start" />} Send</Button>}</span></InputGroupAddon></InputGroup>
    </div></div>
  </section>
}

/**
 * 把 `/threads/{id}/turns` 的分页结果合成为一条时间线条目序列。
 *
 * 每个 Turn 的 items 已按事件顺序排列，但单个 Turn 的 `ordinal` 只在该 Turn 内单调
 * （每轮从 1 重新计数），而分页本身按 Turn 终态从新到旧返回。因此这里先按最旧→最新
 * 还原阅读顺序，再把 `ordinal` 重编成全局单调序号，交给 timeline 投影得到唯一确定的顺序。
 */
function composeTimelineItems(pages: readonly ThreadTurnPage[] | undefined, active: ThreadTurnHistory | null): ThreadItem[] {
  const items: ThreadItem[] = []
  if (pages) {
    for (let pageIndex = pages.length - 1; pageIndex >= 0; pageIndex -= 1) {
      const { turns } = pages[pageIndex]
      for (let turnIndex = turns.length - 1; turnIndex >= 0; turnIndex -= 1) {
        for (const item of turns[turnIndex].items) items.push(item)
      }
    }
  }
  if (active && !pages?.some((page) => page.turns.some((history) => history.turn.id === active.turn.id))) {
    items.push(...active.items)
  }
  return items.map((item, index) => ({ ...item, ordinal: index + 1 }))
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
