import { Bot, CheckCircle2, CircleAlert, User } from "lucide-react"

import { Markdown } from "@/components/markdown"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { ToolCallDetails } from "@/features/session/tool-call-details"

import type { ReviewActivityItem } from "./review-activity"

export function ReviewActivityList({ activity }: { activity: ReviewActivityItem[] }) {
  if (activity.length === 0) return <p className="rounded-lg border border-dashed p-4 text-sm text-muted-foreground">No review activity was archived.</p>
  return <div className="space-y-2.5">{activity.map((item) => {
    switch (item.kind) {
      case "tool":
        return <ToolCallDetails key={item.id} tool={item.tool} />
      case "conclusion":
        return <ReviewConclusion key={item.id} item={item} />
      case "message":
        return <ReviewMessage key={item.id} item={item} />
    }
  })}</div>
}

function ReviewMessage({ item }: { item: Extract<ReviewActivityItem, { kind: "message" }> }) {
  const user = item.role === "user"
  const Icon = user ? User : Bot
  return <article className="flex gap-2.5 rounded-lg border bg-card p-3"><Icon className="mt-0.5 size-4 shrink-0 text-muted-foreground" aria-hidden="true" /><div className="min-w-0 flex-1 space-y-1"><p className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">{user ? "User" : "Reviewer"}</p><Markdown>{item.content}</Markdown></div></article>
}

function ReviewConclusion({ item }: { item: Extract<ReviewActivityItem, { kind: "conclusion" }> }) {
  const failed = item.outcome === "failed" || Boolean(item.error)
  const decision = decisionLabel(item.reviewEvent, item.outcome)
  return (
    <Alert variant={failed ? "destructive" : "default"} className="p-3">
      {failed ? <CircleAlert /> : <CheckCircle2 />}
      <AlertTitle className="flex flex-wrap items-center gap-2">Review conclusion {decision && <Badge variant={failed ? "destructive" : "secondary"}>{decision}</Badge>}</AlertTitle>
      <AlertDescription>{item.error ? <p>{item.error}</p> : item.summary ? <Markdown>{item.summary}</Markdown> : <p>The review completed without a written summary.</p>}</AlertDescription>
    </Alert>
  )
}

function decisionLabel(reviewEvent?: string, outcome?: string) {
  switch (reviewEvent) {
    case "approve": return "Approved"
    case "request_changes": return "Request changes"
    case "comment": return "Commented"
  }
  switch (outcome) {
    case "review_submitted": return "Submitted"
    case "no_eligible_pr": return "No eligible PR"
    case "failed": return "Failed"
    default: return outcome
  }
}
