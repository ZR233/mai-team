/**
 * 时间轴渲染循环：按投影层产出的 TimelineEntry 穷尽分发。
 *
 * ThreadTimeline 是 chat 工作台的入口（接收 ThreadSnapshot）；
 * TimelineEntriesView 是共享的条目列表渲染，供 chat 与 review 活动列表
 * 复用同一分组行为。
 */

import { Bot, ListChecks, LoaderCircle, Sparkles, User } from "lucide-react"
import { useMemo } from "react"

import { Markdown } from "@/components/markdown"
import { Badge } from "@/components/ui/badge"
import type { SkillActivation, ThreadItem, ThreadSnapshot, Turn } from "@/events/thread-events.generated"

import { ReasoningEntry } from "./reasoning-entry"
import { TimelineActivityRail } from "./timeline-activity"
import { buildTimelineEntries, type TimelineEntry } from "./timeline-entries"
import { ToolActivityRow, ToolActivityGroupView } from "./tool-activity-view"
import { buildToolActivity } from "./tool-activity"

export function ThreadTimeline({ snapshot }: { snapshot: ThreadSnapshot | null }) {
  if (!snapshot) return <p className="py-12 text-center text-sm text-muted-foreground">No Thread activity yet.</p>
  return <TimelineEntriesView items={snapshot.items} activeTurn={snapshot.activeTurn} progress={snapshot.runtime?.progress} />
}

export function TimelineEntriesView({ items, activeTurn, progress }: { items: ThreadItem[]; activeTurn?: Turn; progress?: string }) {
  const entries = useMemo(() => buildTimelineEntries(items), [items])
  const activity = activeTurn?.state.kind === "running" ? <ThreadActivityRow phase={activeTurn.state.data.phase} progress={progress} /> : null
  if (entries.length === 0 && !activity) return <p className="py-12 text-center text-sm text-muted-foreground">No Thread activity yet.</p>
  return <div role="feed" aria-label="Conversation timeline" className="mx-auto flex w-full min-w-0 max-w-3xl flex-col gap-3 py-5">{entries.map((entry) => <TimelineEntryView key={entry.key} entry={entry} />)}{activity}</div>
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
        return <TimelineMessage icon={User} label="You" ariaLabel="You message" status={state.data.lifecycle.kind} variant="user"><Markdown>{state.data.text}</Markdown></TimelineMessage>
      }
      return state.data.channel === "final"
        ? <TimelineMessage icon={Bot} label="Mai Team" ariaLabel="Mai Team response" status={state.data.lifecycle.kind} variant="response"><Markdown variant="response">{state.data.text}</Markdown></TimelineMessage>
        : <CommentaryText text={state.data.text} status={state.data.lifecycle.kind} />
    case "thinking":
      return null
    case "plan":
      return <TimelineActivityRail role="group" aria-label="Plan"><article className="flex min-w-0 gap-2 px-1.5 py-1.5"><ListChecks className="mt-1 size-3.5 shrink-0 text-muted-foreground" aria-hidden="true" /><div className="flex min-w-0 flex-1 flex-col gap-1"><span className="text-xs font-medium text-muted-foreground">Plan</span><Markdown variant="auxiliary">{state.data.content}</Markdown></div></article></TimelineActivityRail>
    case "tool":
      return <ToolActivityRow activity={buildToolActivity({ ...item, state })} />
    case "skill":
      return <SkillActivationRow activation={state.data.activation} />
    case "agent":
    case "turn":
    case "inference":
    case "file":
    case "contextCompaction":
      return null
  }
}

function CommentaryText({ text, status }: { text: string; status: string }) {
  return <TimelineActivityRail role="group" aria-label="Commentary"><article className="flex min-w-0 gap-2 px-1.5 py-1.5"><Bot className="mt-1 size-3.5 shrink-0 text-muted-foreground" aria-hidden="true" /><div className="flex min-w-0 flex-1 flex-col gap-1"><div className="flex items-center gap-2"><span className="text-xs font-medium text-muted-foreground">Commentary</span>{status !== "completed" && <Badge variant="secondary">{status}</Badge>}</div><Markdown variant="auxiliary">{text}</Markdown></div></article></TimelineActivityRail>
}

function SkillActivationRow({ activation }: { activation: SkillActivation }) {
  return <TimelineActivityRail><article aria-label={`Skill loaded: ${activation.name}`} className="flex min-h-9 min-w-0 items-center gap-2 px-1.5"><Sparkles className="size-3.5 shrink-0 text-muted-foreground" aria-hidden="true" /><div className="flex min-w-0 flex-1 flex-wrap items-center gap-2"><span className="text-xs font-medium text-muted-foreground">Skill loaded</span><Badge variant="outline">{activation.name}</Badge><span className="min-w-0 truncate text-xs text-muted-foreground">· {skillActivationCauseLabel(activation.cause)} · {activation.source}</span></div></article></TimelineActivityRail>
}

function skillActivationCauseLabel(cause: SkillActivation["cause"]): string {
  switch (cause.kind) {
    case "tool": return "Triggered by tool"
    case "userGesture": return "Selected by user"
  }
}

type ActivePhase = Extract<Turn["state"], { kind: "running" }>["data"]["phase"]

function ThreadActivityRow({ phase, progress }: { phase: ActivePhase; progress?: string }) {
  const label = activityLabel(phase)
  return <TimelineActivityRail><div className="flex min-h-9 min-w-0 items-center gap-2 px-1.5 text-sm text-muted-foreground"><LoaderCircle className="size-3.5 shrink-0 animate-spin motion-reduce:animate-none" aria-hidden="true" /><span className="shrink-0 font-medium">{label}</span>{progress && <span className="truncate">· {progress}</span>}</div></TimelineActivityRail>
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

function TimelineMessage({ icon: Icon, label, ariaLabel, status, variant, children }: { icon: typeof Bot; label: string; ariaLabel: string; status: string; variant: "user" | "response"; children: React.ReactNode }) {
  const user = variant === "user"
  return (
    <article
      aria-label={ariaLabel}
      data-timeline-entry="message"
      data-priority={user ? "normal" : "primary"}
      className={user ? "min-w-0 rounded-xl bg-muted/55 px-4 py-3" : "min-w-0 px-1 py-3 sm:py-4"}
    >
      <header className="mb-2 flex items-center gap-2 text-xs text-muted-foreground">
        <Icon className="size-3.5 shrink-0" aria-hidden="true" />
        <span className="font-medium text-foreground">{label}</span>
        {status !== "completed" && <Badge variant="secondary">{status}</Badge>}
      </header>
      {children}
    </article>
  )
}
