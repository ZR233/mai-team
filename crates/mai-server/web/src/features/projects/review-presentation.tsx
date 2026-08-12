import { CircleAlert } from "lucide-react"

import type { ReviewJobSummary } from "@/api/product-types"
import { Badge } from "@/components/ui/badge"

import { reviewSkipReasonLabel } from "./review-job-model"

export function ReviewOutcome({ job, merged = false }: { job: ReviewJobSummary; merged?: boolean }) {
  if (merged) return <Badge variant="secondary">Merged</Badge>
  const event = job.submission_receipt?.event
  if (event === "approve") return <Badge variant="secondary">Approved</Badge>
  if (event === "request_changes") return <Badge variant="destructive">Request changes</Badge>
  if (event === "comment") return <Badge variant="outline">Commented</Badge>
  if (job.status === "failed") return <Badge variant="destructive">Failed</Badge>
  if (job.status === "skipped") return <Badge variant="outline">{reviewSkipReasonLabel(job.skip_reason)}</Badge>
  if (job.status === "superseded") return <Badge variant="outline">Superseded</Badge>
  if (job.status === "cancelled") return <Badge variant="outline">Cancelled</Badge>
  return <span className="text-sm text-muted-foreground">—</span>
}

export function EnvironmentWarningBadge({ job }: { job: ReviewJobSummary }) {
  if (!job.environment_warning) return null
  return <Badge variant="outline" className="border-amber-500/60 text-amber-700 dark:text-amber-300"><CircleAlert /> Image update failed</Badge>
}
