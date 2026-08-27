/**
 * 时间轴渲染循环：按投影层产出的 TimelineEntry 穷尽分发。
 *
 * ThreadTimeline 是 chat 工作台的入口（接收 ThreadSnapshot）；
 * TimelineEntriesView 是共享的条目列表渲染，供 chat 与 review 活动列表
 * 复用同一分组行为。
 */

import { Bot, ListChecks, LoaderCircle, User } from "lucide-react"

import { Markdown } from "@/components/markdown"
import { Badge } from "@/components/ui/badge"
import type { ThreadItem, ThreadSnapshot, Turn } from "@/events/thread-events.generated"

import { ReasoningEntry } from "./reasoning-entry"
import { buildTimelineEntries, type TimelineEntry } from "./timeline-entries"
import { ToolActivityRow, ToolActivityGroupView } from "./tool-activity-view"
import { buildToolActivity } from "./tool-activity"

export function ThreadTimeline({ snapshot }: { snapshot: ThreadSnapshot | null }) {
  if (!snapshot) return <p className="py-12 text-center text-sm text-muted-foreground">No Thread activity yet.</p>
  return <TimelineEntriesView items={snapshot.items} activeTurn={snapshot.activeTurn} progress={snapshot.runtime?.progress} />
}

export function TimelineEntriesView({ items, activeTurn, progress }: { items: ThreadItem[]; activeTurn?: Turn; progress?: string }) {
  const entries = buildTimelineEntries(items)
  const activity = activeTurn?.state.kind === "running" ? <ThreadActivityRow phase={activeTurn.state.data.phase} progress={progress} /> : null
  if (entries.length === 0 && !activity) return <p className="py-12 text-center text-sm text-muted-foreground">No Thread activity yet.</p>
  return <div className="space-y-3 py-5">{entries.map((entry) => <TimelineEntryView key={entry.key} entry={entry} />)}{activity}</div>
}

function TimelineEntryView({ entry }: { entry: TimelineEntry }) {
  switch (entry.kind) {
    case "item":
      return <ThreadItemCard item={entry.item} />
    case "tool":
      return <ToolActivityRow activity={entry.activity} />
    case "toolGroup":
      return <ToolActivityGroupView group={entry.group} />
    case "reasoningGroup":
      return <ReasoningEntry group={entry.group} />
  }
}

function ThreadItemCard({ item }: { item: ThreadItem }) {
  const state = item.state
  switch (state.kind) {
    case "text":
      if (state.data.channel === "user") {
        return <TimelineCard icon={User} label="User" status={state.data.lifecycle.kind}><Markdown>{state.data.text}</Markdown></TimelineCard>
      }
      return state.data.channel === "final"
        ? <TimelineCard icon={Bot} label="Final" status={state.data.lifecycle.kind}><Markdown>{state.data.text}</Markdown></TimelineCard>
        : <CommentaryText text={state.data.text} status={state.data.lifecycle.kind} />
    case "thinking":
      return null
    case "plan":
      return <article className="flex gap-3 rounded-md border-l-2 bg-muted/20 px-3 py-2.5"><ListChecks className="mt-0.5 size-4 shrink-0 text-muted-foreground" /><div className="min-w-0 flex-1 space-y-1"><span className="text-xs font-medium text-muted-foreground">Plan</span><Markdown>{state.data.content}</Markdown></div></article>
    case "tool":
      return <ToolActivityRow activity={buildToolActivity({ ...item, state })} />
    case "agent":
    case "turn":
    case "inference":
    case "skill":
    case "file":
    case "contextCompaction":
      return null
  }
}

function CommentaryText({ text, status }: { text: string; status: string }) {
  return <article className="flex gap-3 px-1 py-1.5"><Bot className="mt-0.5 size-4 shrink-0 text-muted-foreground" /><div className="min-w-0 flex-1 space-y-1"><div className="flex items-center gap-2"><span className="text-xs font-medium text-muted-foreground">Commentary</span>{status !== "completed" && <Badge variant="secondary">{status}</Badge>}</div><Markdown>{text}</Markdown></div></article>
}

type ActivePhase = Extract<Turn["state"], { kind: "running" }>["data"]["phase"]

function ThreadActivityRow({ phase, progress }: { phase: ActivePhase; progress?: string }) {
  const label = activityLabel(phase)
  return <div className="flex min-h-9 items-center gap-2 rounded-md px-1.5 text-sm text-muted-foreground"><LoaderCircle className="size-4 animate-spin motion-reduce:animate-none" /><span className="font-medium">{label}</span>{progress && <span className="truncate">· {progress}</span>}</div>
}

function activityLabel(phase: ActivePhase): string {
  switch (phase) {
    case "preparing": return "Preparing context"
    case "thinking": return "Thinking"
    case "responding": return "Writing response"
    case "planning": return "Planning"
    case "runningTool": return "Running tools"
    case "persisting": return "Saving"
  }
}

function TimelineCard({ icon: Icon, label, status, children }: { icon: typeof Bot; label: string; status: string; children: React.ReactNode }) {
  return <article className="flex gap-3 rounded-lg border bg-card p-3"><Icon className="mt-0.5 size-4 shrink-0 text-muted-foreground" /><div className="min-w-0 flex-1 space-y-2"><div className="flex items-center gap-2"><span className="text-xs font-medium">{label}</span><Badge variant="secondary">{status}</Badge></div>{children}</div></article>
}
