/**
 * 时间轴渲染循环：按投影层产出的 TimelineEntry 穷尽分发。
 *
 * ThreadTimeline 是 chat 工作台的入口（接收 ThreadSnapshot）；
 * TimelineEntriesView 是共享的条目列表渲染，供 chat 与 review 活动列表
 * 复用同一分组行为。
 */

import { Bot, FileText, ListChecks, User } from "lucide-react"

import { Markdown } from "@/components/markdown"
import { Badge } from "@/components/ui/badge"
import type { ThreadItem, ThreadSnapshot } from "@/events/thread-events.generated"

import { ReasoningEntry } from "./reasoning-entry"
import { buildTimelineEntries, type TimelineEntry } from "./timeline-entries"
import { ToolActivityRow, ToolActivityGroupView } from "./tool-activity-view"
import { buildToolActivity } from "./tool-activity"

export function ThreadTimeline({ snapshot }: { snapshot: ThreadSnapshot | null }) {
  if (!snapshot || snapshot.items.length === 0) return <p className="py-12 text-center text-sm text-muted-foreground">No Thread activity yet.</p>
  return <TimelineEntriesView items={snapshot.items} />
}

export function TimelineEntriesView({ items }: { items: ThreadItem[] }) {
  const entries = buildTimelineEntries(items)
  return <div className="space-y-3 py-5">{entries.map((entry) => <TimelineEntryView key={entry.key} entry={entry} />)}</div>
}

function TimelineEntryView({ entry }: { entry: TimelineEntry }) {
  switch (entry.kind) {
    case "item":
      return <ThreadItemCard item={entry.item} />
    case "tool":
      return <ToolActivityRow activity={entry.activity} />
    case "toolGroup":
      return <ToolActivityGroupView group={entry.group} />
  }
}

function ThreadItemCard({ item }: { item: ThreadItem }) {
  const content = item.content
  switch (content.type) {
    case "userMessage":
      return <TimelineCard icon={User} label="User" status={item.status}><Markdown>{content.text ?? ""}</Markdown></TimelineCard>
    case "agentMessage":
      return <TimelineCard icon={Bot} label={content.channel === "final" ? "Final" : "Commentary"} status={item.status}><Markdown>{content.text ?? ""}</Markdown></TimelineCard>
    case "reasoning":
      return <ReasoningEntry item={{ ...item, content }} />
    case "plan":
      return <TimelineCard icon={ListChecks} label="Plan" status={item.status}><Markdown>{content.content ?? ""}</Markdown></TimelineCard>
    case "toolCall":
      return <ToolActivityRow activity={buildToolActivity({ ...item, content })} />
    case "file":
      return <TimelineCard icon={FileText} label="File" status={item.status}><code className="text-xs">{content.path}</code></TimelineCard>
    case "contextCompaction":
      return <p className="text-center text-xs text-muted-foreground">Context compacted: {content.beforeTokens.toLocaleString()} → {content.afterTokens.toLocaleString()} tokens</p>
  }
}

function TimelineCard({ icon: Icon, label, status, children }: { icon: typeof Bot; label: string; status: string; children: React.ReactNode }) {
  return <article className="flex gap-3 rounded-lg border bg-card p-3"><Icon className="mt-0.5 size-4 shrink-0 text-muted-foreground" /><div className="min-w-0 flex-1 space-y-2"><div className="flex items-center gap-2"><span className="text-xs font-medium">{label}</span><Badge variant="secondary">{status}</Badge></div>{children}</div></article>
}
