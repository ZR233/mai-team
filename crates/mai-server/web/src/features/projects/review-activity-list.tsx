import { CheckCircle2, CircleAlert } from "lucide-react"

import { Markdown } from "@/components/markdown"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { TimelineEntriesView } from "@/features/thread/timeline"

import type { ReviewActivity, ReviewConclusionView } from "./review-activity"

export function ReviewActivityList({ activity }: { activity: ReviewActivity }) {
  return <div className="flex flex-col gap-4">
    {activity.historyStatus === "pl_v2_archived"
      ? <Alert><CircleAlert /><AlertTitle>PL v2 升级离线归档</AlertTitle><AlertDescription>此 Review 的旧 Timeline 已保存到部署归档，不再由当前服务在线读取。{activity.historyArchiveId && <> 归档标识：<code>{activity.historyArchiveId}</code></>}</AlertDescription></Alert>
      : activity.items.length === 0
      ? <p className="rounded-lg border border-dashed p-4 text-sm text-muted-foreground">No review activity was archived.</p>
      : <TimelineEntriesView items={activity.items} />}
    <ReviewConclusion item={activity.conclusion} />
  </div>
}

function ReviewConclusion({ item }: { item: ReviewConclusionView }) {
  const failed = item.outcome === "failed" || Boolean(item.error)
  const decision = decisionLabel(item.reviewEvent, item.outcome)
  if (failed) return (
    <Alert variant="destructive" className="p-3">
      <CircleAlert />
      <AlertTitle className="flex flex-wrap items-center gap-2">Review conclusion {decision && <Badge variant="destructive">{decision}</Badge>}</AlertTitle>
      <AlertDescription>{item.error ? <p>{item.error}</p> : item.summary ? <Markdown>{item.summary}</Markdown> : <p>The review completed without a written summary.</p>}</AlertDescription>
    </Alert>
  )

  return (
    <section aria-label="Review conclusion" className="flex flex-col gap-2 border-t pt-4">
      <div className="flex flex-wrap items-center gap-2">
        <CheckCircle2 className="size-4 text-muted-foreground" aria-hidden="true" />
        <h4 className="text-sm font-medium">Review conclusion</h4>
        {decision && <Badge variant="secondary">{decision}</Badge>}
      </div>
      {item.summary ? <Markdown>{item.summary}</Markdown> : <p className="text-sm text-muted-foreground">The review completed without a written summary.</p>}
    </section>
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
