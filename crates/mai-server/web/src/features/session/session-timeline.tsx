import { Bot, Brain, CheckCircle2, ChevronDown, CircleDot, Download, FileOutput, TerminalSquare, User } from "lucide-react"
import { memo, useState } from "react"

import { Markdown } from "@/components/markdown"
import { EmptyState } from "@/components/page-state"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import type { NormalizedSession } from "@/events/session-reducer"
import type { SessionPart, SessionTimelineEvent } from "@/events/session-events.generated"
import { cn } from "@/lib/utils"

export function SessionTimeline({ view }: { view: NormalizedSession }) {
  const hasTimeline = view.messageOrder.length > 0 || view.timelineEvents.length > 0
  if (!hasTimeline) {
    return <EmptyState title="No messages yet" description="Send a message to start this session." />
  }
  return (
    <div className="space-y-7 py-6">
      {view.messageOrder.map((messageId) => {
        const message = view.messages[messageId]
        if (!message) return null
        const parts = (view.partOrderByMessage[messageId] ?? []).map((partId) => view.parts[partId]).filter(Boolean)
        return <MessageRow key={messageId} role={message.role} parts={parts} status={message.status} />
      })}
      {view.timelineEvents.map((event) => <TimelineFact key={event.eventId} event={event.kind} />)}
    </div>
  )
}

const MessageRow = memo(function MessageRow({ role, parts, status }: { role: string; parts: SessionPart[]; status: string }) {
  const user = role === "user"
  return (
    <article className="timeline-row flex gap-3" style={{ contentVisibility: "auto", containIntrinsicSize: "120px" }}>
      <div className={cn(
        "mt-0.5 grid size-8 shrink-0 place-items-center rounded-full",
        user ? "bg-muted text-muted-foreground" : "border bg-background text-foreground",
      )}>{user ? <User className="size-4" /> : <Bot className="size-4" />}</div>
      <div className="min-w-0 flex-1">
        <div className="mb-2 flex items-center gap-2">
          <span className="text-sm font-semibold">{user ? "You" : "Assistant"}</span>
          {status !== "completed" && <Badge variant="outline" className="h-4 px-1.5 text-[10px] capitalize">{status}</Badge>}
        </div>
        <div className={cn("flex flex-col gap-3", user && "inline-block max-w-3xl rounded-lg bg-muted px-4 py-3")}>
          {parts.map((part) => <PartRow key={part.partId} part={part} />)}
        </div>
      </div>
    </article>
  )
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
  const [open, setOpen] = useState(false)
  if (part.content.type !== "tool") return null
  const tool = part.content.tool
  const complete = part.status === "completed"
  return (
    <Collapsible open={open} onOpenChange={setOpen} className="rounded-lg border bg-card">
      <CollapsibleTrigger className="flex w-full items-center gap-2 px-3 py-2 text-left">
        <TerminalSquare className="size-4 text-muted-foreground" />
        <code className="min-w-0 flex-1 truncate text-xs font-medium">{tool.name}</code>
        <Badge variant={complete ? "outline" : "secondary"} className="capitalize">{part.status}</Badge>
        {complete ? <CheckCircle2 className="size-3.5" /> : <CircleDot className="size-3.5 animate-pulse" />}
        <ChevronDown className={cn("size-3.5 text-muted-foreground transition-transform", open && "rotate-180")} />
      </CollapsibleTrigger>
      <CollapsibleContent className="border-t px-3 py-3">
        <div className="flex flex-col gap-2">
          {tool.arguments && <pre className="max-h-52 overflow-auto whitespace-pre-wrap break-words rounded-md bg-foreground p-3 text-xs text-background">{tool.arguments}</pre>}
          {tool.result && <pre className="max-h-80 overflow-auto whitespace-pre-wrap break-words rounded-md bg-muted p-3 text-xs text-foreground">{tool.result}</pre>}
          {tool.outputArtifacts && tool.outputArtifacts.length > 0 && <div className="flex flex-col gap-1">{tool.outputArtifacts.map((artifact, index) => <ToolArtifact key={`${artifact.id || artifact.path || "artifact"}:${index}`} artifact={artifact} />)}</div>}
        </div>
      </CollapsibleContent>
    </Collapsible>
  )
}

function ToolArtifact({ artifact }: { artifact: Record<string, unknown> }) {
  const id = typeof artifact.id === "string" ? artifact.id : null
  const label = String(artifact.name || artifact.path || artifact.filename || "Tool output")
  const content = <><FileOutput className="size-3.5" /><span className="min-w-0 flex-1 truncate">{label}</span>{id && <Download className="size-3.5" />}</>
  return id
    ? <a href={`/artifacts/${encodeURIComponent(id)}/download`} download className="flex items-center gap-2 rounded-md border px-2.5 py-2 text-xs hover:bg-muted">{content}</a>
    : <div className="flex items-center gap-2 rounded-md border px-2.5 py-2 text-xs text-muted-foreground">{content}</div>
}

function TimelineFact({ event }: { event: SessionTimelineEvent["kind"] }) {
  if (event.type === "todoListChanged") {
    const snapshot = event.snapshot
    return (
      <div className="ml-11 rounded-lg border bg-muted/40 px-4 py-3">
        <div className="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Working list</div>
        <div className="flex flex-col gap-1.5">
          {snapshot.items.map((item, index) => (
            <div key={`${item.step}:${index}`} className="flex items-start gap-2 text-sm">
              <CheckCircle2 className={cn("mt-0.5 size-4 text-muted-foreground", item.status === "inProgress" && "animate-pulse text-foreground", item.status === "completed" && "text-foreground")} />
              <span>{item.step}</span>
            </div>
          ))}
        </div>
      </div>
    )
  }
  return null
}
