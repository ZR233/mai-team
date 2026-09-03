import { Radar } from "lucide-react"

import type { ProjectReviewDiscoverySnapshot } from "@/api/product-types"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"

import { formatReviewDate } from "./review-formatters"

interface ReviewDiscoveryCardProps {
  snapshot?: ProjectReviewDiscoverySnapshot
  loading: boolean
  error?: Error | null
  onRetry(): void
}

const stateLabels: Record<ProjectReviewDiscoverySnapshot["state"], string> = {
  disabled: "Disabled",
  idle: "Idle",
  scanning: "Scanning",
  partial: "Partial",
  backoff: "Backoff",
}

export function ReviewDiscoveryCard({ snapshot, loading, error, onRetry }: ReviewDiscoveryCardProps) {
  return <Card size="sm" aria-label="PR discovery status">
    <CardHeader className="gap-2">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <CardTitle className="flex items-center gap-2"><Radar className="size-4 text-muted-foreground" aria-hidden />PR discovery</CardTitle>
        {snapshot && <Badge variant={snapshot.state === "backoff" || snapshot.state === "partial" ? "destructive" : "secondary"}>{stateLabels[snapshot.state]}</Badge>}
      </div>
      <CardDescription>Independent fallback scan for open pull requests and pending CI.</CardDescription>
    </CardHeader>
    <CardContent className="flex flex-col gap-3">
      {loading && !snapshot && <p className="text-sm text-muted-foreground">Loading discovery status…</p>}
      {error && !snapshot && <div className="flex items-center justify-between gap-3"><p className="text-sm text-destructive">{error.message}</p><Button size="sm" variant="outline" onClick={onRetry}>Retry</Button></div>}
      {snapshot && <>
        <dl className="grid grid-cols-2 gap-x-4 gap-y-2 text-sm sm:grid-cols-4">
          <DiscoveryMetric label="Last scan" value={snapshot.last_completed_at ? formatReviewDate(snapshot.last_completed_at) : snapshot.state === "scanning" ? "In progress" : "—"} />
          <DiscoveryMetric label="Next scan" value={snapshot.next_scan_at ? formatReviewDate(snapshot.next_scan_at) : "—"} />
          <DiscoveryMetric label="Scanned" value={snapshot.scanned} />
          <DiscoveryMetric label="Eligible" value={snapshot.eligible} />
          <DiscoveryMetric label="Queued" value={snapshot.queued} />
          <DiscoveryMetric label="Deduped" value={snapshot.deduped} />
          <DiscoveryMetric label="CI watched" value={snapshot.watched} />
          <DiscoveryMetric label="Closed" value={snapshot.closed} />
          <DiscoveryMetric label="Draft" value={snapshot.draft} />
          <DiscoveryMetric label="Already reviewed" value={snapshot.already_reviewed} />
          <DiscoveryMetric label="Suppressed" value={snapshot.suppressed} />
        </dl>
        {snapshot.errors > 0 && <p className="text-xs text-muted-foreground">{snapshot.errors} candidate {snapshot.errors === 1 ? "error" : "errors"}; other candidates were still processed.</p>}
        {snapshot.last_error && <p className="whitespace-pre-wrap rounded-md border border-destructive/40 bg-destructive/5 p-2 text-xs text-destructive">{snapshot.last_error}</p>}
      </>}
    </CardContent>
  </Card>
}

function DiscoveryMetric({ label, value }: { label: string; value: string | number }) {
  return <div className="min-w-0"><dt className="text-xs text-muted-foreground">{label}</dt><dd className="truncate font-medium tabular-nums">{value}</dd></div>
}
