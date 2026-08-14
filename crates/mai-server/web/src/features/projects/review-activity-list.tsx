import { CheckCircle2, CircleAlert } from "lucide-react"

import { Markdown } from "@/components/markdown"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { TimelineEntriesView } from "@/features/thread/timeline"

import type { ReviewActivity, ReviewConclusionView } from "./review-activity"

export function ReviewActivityList({ activity }: { activity: ReviewActivity }) {
  return <div className="space-y-2.5">
    {activity.items.length === 0
      ? <p className="rounded-lg border border-dashed p-4 text-sm text-muted-foreground">No review activity was archived.</p>
      : <TimelineEntriesView items={activity.items} />}
    <ReviewConclusion item={activity.conclusion} />
  </div>
}

function ReviewConclusion({ item }: { item: ReviewConclusionView }) {
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
