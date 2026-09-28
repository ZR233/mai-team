import { CheckCircle2, CircleAlert } from "lucide-react"
import { useState } from "react"

import { Markdown } from "@/components/markdown"
import { PagePagination } from "@/components/page-pagination"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { TimelineEntriesView } from "@/features/thread/timeline"

import type { ReviewActivity, ReviewConclusionView } from "./review-activity"

const ACTIVITY_PAGE_SIZE = 40

export function ReviewActivityList({ activity }: { activity: ReviewActivity }) {
  const [selectedPage, setSelectedPage] = useState<number | null>(null)
  const active = activity.status === "syncing" || activity.status === "running"
  const totalPages = Math.max(1, Math.ceil(activity.items.length / ACTIVITY_PAGE_SIZE))
  const page = Math.min(selectedPage ?? totalPages, totalPages)
  const items = activity.items.slice((page - 1) * ACTIVITY_PAGE_SIZE, page * ACTIVITY_PAGE_SIZE)
  return <div className="flex flex-col gap-4">
    {activity.items.length === 0 && !active
      ? <p className="rounded-lg border border-dashed p-4 text-sm text-muted-foreground">No committed PL turn activity is available for this attempt.</p>
      : <TimelineEntriesView items={items} activeTurn={active && page === totalPages ? activity.turn : undefined} />}
    {active && activity.items.length === 0 && <p className="text-xs text-muted-foreground">The PL turn is still running. Its chat will appear as effects are committed.</p>}
    {activity.items.length > ACTIVITY_PAGE_SIZE && <PagePagination page={page} totalPages={totalPages} onPageChange={(next) => setSelectedPage(next === totalPages ? null : next)} label="Review activity pages" />}
    {active ? <p className="text-xs text-muted-foreground">Review conclusion will appear when this attempt finishes.</p> : <ReviewConclusion item={activity.conclusion} />}
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
