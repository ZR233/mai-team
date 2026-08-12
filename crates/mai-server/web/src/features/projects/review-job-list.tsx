import { ExternalLink, Eye, MoreHorizontal, RefreshCw } from "lucide-react"

import type { PullRequestReviewSummary } from "@/api/product-types"
import { StatusBadge } from "@/components/status"
import { Button } from "@/components/ui/button"
import { DropdownMenu, DropdownMenuContent, DropdownMenuGroup, DropdownMenuItem, DropdownMenuTrigger } from "@/components/ui/dropdown-menu"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"

import { formatReviewDate, pullRequestUrl } from "./review-formatters"
import { reviewSkipReasonLabel } from "./review-job-model"
import { EnvironmentWarningBadge, ReviewOutcome } from "./review-presentation"

interface ReviewListProps {
  reviews: PullRequestReviewSummary[]
  repository: string
  onDetails(review: PullRequestReviewSummary): void
  onRereview(pr: number): void
  pending: boolean
}

export function ReviewJobList(props: ReviewListProps) {
  return <>
    <ReviewTable {...props} />
    <ReviewMobileList {...props} />
  </>
}

function ReviewTable({ reviews, repository, onDetails, onRereview, pending }: ReviewListProps) {
  return <div className="hidden overflow-hidden rounded-lg border lg:block"><Table><TableHeader><TableRow><TableHead>Status</TableHead><TableHead>Pull request</TableHead><TableHead>Result</TableHead><TableHead>Attempts</TableHead><TableHead>Created</TableHead><TableHead>Next action</TableHead><TableHead className="w-24 text-right">Actions</TableHead></TableRow></TableHeader><TableBody>{reviews.map((review) => {
    const job = review.latest_job
    return <TableRow key={review.pr} className="cursor-pointer" onClick={() => onDetails(review)}><TableCell><span className="flex flex-col items-start gap-1"><StatusBadge status={job.status} /><EnvironmentWarningBadge job={job} /></span></TableCell><TableCell><button type="button" className="text-left" onClick={(event) => { event.stopPropagation(); onDetails(review) }}><span className="block font-medium">PR #{review.pr}</span><span className="text-xs text-muted-foreground">{review.history_count} history {review.history_count === 1 ? "record" : "records"}</span></button></TableCell><TableCell><ReviewOutcome job={job} lifecycleState={review.lifecycle_state} /></TableCell><TableCell className="text-muted-foreground tabular-nums">{job.attempt_count}/{job.max_attempts}</TableCell><TableCell className="text-muted-foreground">{formatReviewDate(job.created_at)}</TableCell><TableCell className="text-muted-foreground">{job.next_attempt_at ? formatReviewDate(job.next_attempt_at) : "—"}</TableCell><TableCell onClick={(event) => event.stopPropagation()}><ReviewActions review={review} repository={repository} onDetails={onDetails} onRereview={onRereview} pending={pending} /></TableCell></TableRow>
  })}</TableBody></Table></div>
}

function ReviewMobileList({ reviews, repository, onDetails, onRereview, pending }: ReviewListProps) {
  return <div className="divide-y overflow-hidden rounded-lg border lg:hidden">{reviews.map((review) => {
    const job = review.latest_job
    return <div key={review.pr} className="flex items-center gap-3 p-3"><StatusBadge status={job.status} /><button type="button" className="min-w-0 flex-1 text-left" onClick={() => onDetails(review)}><span className="flex flex-wrap items-center gap-2"><span className="font-medium">PR #{review.pr}</span><ReviewOutcome job={job} lifecycleState={review.lifecycle_state} /><EnvironmentWarningBadge job={job} /></span><span className="block truncate text-xs text-muted-foreground">{job.failure?.message || (job.status === "skipped" ? reviewSkipReasonLabel(job.skip_reason) : `${job.attempt_count}/${job.max_attempts} attempts · ${review.history_count} history records · ${formatReviewDate(job.created_at)}`)}</span></button><ReviewActions review={review} repository={repository} onDetails={onDetails} onRereview={onRereview} pending={pending} /></div>
  })}</div>
}

function ReviewActions({ review, repository, onDetails, onRereview, pending }: Omit<ReviewListProps, "reviews"> & { review: PullRequestReviewSummary }) {
  const url = pullRequestUrl(repository, review.pr)
  return <div className="flex justify-end gap-1">{url && <Button asChild variant="ghost" size="icon" aria-label={`Open PR #${review.pr}`}><a href={url} target="_blank" rel="noreferrer"><ExternalLink data-icon="inline-start" /></a></Button>}<DropdownMenu><DropdownMenuTrigger asChild><Button variant="ghost" size="icon" aria-label={`Actions for PR #${review.pr}`}><MoreHorizontal data-icon="inline-start" /></Button></DropdownMenuTrigger><DropdownMenuContent align="end"><DropdownMenuGroup><DropdownMenuItem onSelect={() => onDetails(review)}><Eye /> View details</DropdownMenuItem>{review.lifecycle_state === "open" && <DropdownMenuItem disabled={pending} onSelect={() => onRereview(review.pr)}><RefreshCw /> Re-review</DropdownMenuItem>}</DropdownMenuGroup></DropdownMenuContent></DropdownMenu></div>
}
