import { Bot, Brain, FileText, ListChecks, User, Wrench } from "lucide-react"

import { Markdown } from "@/components/markdown"
import { Badge } from "@/components/ui/badge"
import type { ThreadItem, ThreadSnapshot } from "@/events/thread-events.generated"

export function ThreadTimeline({ snapshot }: { snapshot: ThreadSnapshot | null }) {
  if (!snapshot || snapshot.items.length === 0) return <p className="py-12 text-center text-sm text-muted-foreground">No Thread activity yet.</p>
  return <div className="space-y-3 py-5">{snapshot.items.map((item) => <ThreadTimelineItem key={item.id} item={item} />)}</div>
}

export function ThreadTimelineItem({ item }: { item: ThreadItem }) {
  const content = item.content
  switch (content.type) {
    case "userMessage":
      return <TimelineCard icon={User} label="User" status={item.status}><Markdown>{content.text ?? ""}</Markdown></TimelineCard>
    case "agentMessage":
      return <TimelineCard icon={Bot} label={content.channel === "final" ? "Final" : "Commentary"} status={item.status}><Markdown>{content.text ?? ""}</Markdown></TimelineCard>
    case "reasoning":
      return <TimelineCard icon={Brain} label="Reasoning" status={item.status}><Markdown>{[...(content.summary ?? []), ...(content.content ?? [])].join("\n\n")}</Markdown></TimelineCard>
    case "plan":
      return <TimelineCard icon={ListChecks} label="Plan" status={item.status}><Markdown>{content.content ?? ""}</Markdown></TimelineCard>
    case "toolCall":
      return <TimelineCard icon={Wrench} label={content.tool.name || "Tool"} status={item.status} error={item.error}>
        {content.tool.arguments && <CodeBlock label="Arguments" value={content.tool.arguments} />}
        {content.tool.result && <CodeBlock label="Result" value={content.tool.result} />}
      </TimelineCard>
    case "file":
      return <TimelineCard icon={FileText} label="File" status={item.status}><code className="text-xs">{content.path}</code></TimelineCard>
    case "contextCompaction":
      return <p className="text-center text-xs text-muted-foreground">Context compacted: {content.beforeTokens.toLocaleString()} → {content.afterTokens.toLocaleString()} tokens</p>
  }
}

function TimelineCard({ icon: Icon, label, status, error, children }: { icon: typeof Bot; label: string; status: string; error?: string; children: React.ReactNode }) {
  return <article className="flex gap-3 rounded-lg border bg-card p-3"><Icon className="mt-0.5 size-4 shrink-0 text-muted-foreground" /><div className="min-w-0 flex-1 space-y-2"><div className="flex items-center gap-2"><span className="text-xs font-medium">{label}</span><Badge variant="secondary">{status}</Badge></div>{children}{error && <p className="text-xs text-destructive">{error}</p>}</div></article>
}

function CodeBlock({ label, value }: { label: string; value: string }) {
  return <details className="rounded-md border bg-muted/30" open={label === "Result"}><summary className="cursor-pointer px-3 py-2 text-xs font-medium">{label}</summary><pre className="max-h-96 overflow-auto border-t p-3 text-xs whitespace-pre-wrap">{value}</pre></details>
}
