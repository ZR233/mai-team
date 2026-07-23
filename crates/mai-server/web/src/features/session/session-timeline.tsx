import { Bot, Brain, ChevronDown, User } from "lucide-react"
import { memo, useState } from "react"

import { Markdown } from "@/components/markdown"
import { EmptyState } from "@/components/page-state"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import type { NormalizedSession } from "@/events/session-reducer"
import type { SessionPart } from "@/events/session-events.generated"
import { cn } from "@/lib/utils"

import { buildTimelineRenderItems, type TimelineRenderItem } from "./timeline-presentation"
import { ToolActivityGroup } from "./tool-activity-group"
import { ToolCallDetails } from "./tool-call-details"

export function SessionTimeline({ view }: { view: NormalizedSession }) {
  if (view.messageOrder.length === 0) {
    return <EmptyState title="No messages yet" description="Send a message to start this session." />
  }
  return (
    <div className="flex w-full min-w-0 max-w-full flex-col gap-7 overflow-hidden py-6">
      {view.messageOrder.map((messageId) => {
        const message = view.messages[messageId]
        if (!message) return null
        const parts = (view.partOrderByMessage[messageId] ?? []).map((partId) => view.parts[partId]).filter(Boolean)
        return <MessageRow key={messageId} role={message.role} parts={parts} status={message.status} />
      })}
    </div>
  )
}

const MessageRow = memo(function MessageRow({ role, parts, status }: { role: string; parts: SessionPart[]; status: string }) {
  const user = role === "user"
  const items = buildTimelineRenderItems(parts)
  return (
    <article className="timeline-row flex w-full min-w-0 max-w-full gap-3 overflow-hidden" style={{ contentVisibility: "auto", containIntrinsicSize: "120px" }}>
      <div className={cn(
        "mt-0.5 grid size-8 shrink-0 place-items-center rounded-full",
        user ? "bg-muted text-muted-foreground" : "border bg-background text-foreground",
      )}>{user ? <User className="size-4" /> : <Bot className="size-4" />}</div>
      <div className="min-w-0 max-w-full flex-1 overflow-hidden">
        <div className="mb-2 flex items-center gap-2">
          <span className="text-sm font-semibold">{user ? "You" : "Assistant"}</span>
          {status !== "completed" && <Badge variant="outline" className="h-4 px-1.5 text-[10px] capitalize">{status}</Badge>}
        </div>
        <div className={cn(
          user ? "inline-block max-w-3xl rounded-lg bg-muted px-4 py-3" : "flex min-w-0 max-w-full flex-col gap-3 overflow-hidden",
        )}>
          {items.map((item) => <TimelineItemRow key={item.key} item={item} />)}
        </div>
      </div>
    </article>
  )
})

const TimelineItemRow = memo(function TimelineItemRow({ item }: { item: TimelineRenderItem }) {
  return item.kind === "toolGroup"
    ? <ToolActivityGroup parts={item.parts} />
    : <PartRow part={item.part} />
})

const PartRow = memo(function PartRow({ part }: { part: SessionPart }) {
  switch (part.content.type) {
    case "text":
      return part.content.text ? <Markdown>{part.content.text}</Markdown> : null
    case "reasoning":
      return <ReasoningRow text={part.content.text ?? ""} status={part.status} />
    case "tool":
      return <ToolRow part={part} />
    case "plan":
      return <div className="rounded-lg border bg-muted/40 p-3"><Markdown>{part.content.content ?? ""}</Markdown></div>
    case "agent":
      return <div className="rounded-lg border px-3 py-2 text-xs text-muted-foreground">Subagent · {String(part.content.agent.role ?? "agent")} · {String(part.content.agent.status ?? part.status)}</div>
    case "file":
      return <div className="font-mono text-xs text-muted-foreground">{part.content.path}</div>
    case "inference":
      return <div className="text-xs text-muted-foreground">Model · {part.content.model}</div>
    case "turn":
      return null
  }
})

function ReasoningRow({ text, status }: { text: string; status: string }) {
  const [open, setOpen] = useState(status !== "completed")
  if (!text) return null
  return (
    <Collapsible open={open} onOpenChange={setOpen}>
      <CollapsibleTrigger asChild>
        <Button variant="ghost" size="sm" className="-ml-2"><Brain data-icon="inline-start" /> Reasoning <ChevronDown data-icon="inline-end" className={cn("transition-transform", open && "rotate-180")} /></Button>
      </CollapsibleTrigger>
      <CollapsibleContent className="border-l-2 pl-3 text-muted-foreground"><Markdown>{text}</Markdown></CollapsibleContent>
    </Collapsible>
  )
}

function ToolRow({ part }: { part: SessionPart }) {
  if (part.content.type !== "tool") return null
  const tool = part.content.tool
  return <ToolCallDetails tool={{ ...tool, status: part.status, error: part.error }} />
}
