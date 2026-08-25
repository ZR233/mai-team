import { useQuery, type UseQueryResult } from "@tanstack/react-query"
import { ChevronDown, CircleAlert, ExternalLink, RefreshCw } from "lucide-react"
import { useEffect, useState } from "react"

import type { PullRequestReviewHistoryItem, PullRequestReviewHistoryPage, PullRequestReviewSummary, ReviewJobDetail, ReviewRunDetail, ReviewRunSummary } from "@/api/product-types"
import { projectPullRequestReviewHistoryQuery, projectReviewJobQuery, projectReviewRunQuery } from "@/api/queries"
import { Markdown } from "@/components/markdown"
import { PagePagination } from "@/components/page-pagination"
import { ErrorState, LoadingState } from "@/components/page-state"
import { StatusBadge } from "@/components/status"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { Drawer, DrawerContent, DrawerDescription, DrawerFooter, DrawerHeader, DrawerTitle } from "@/components/ui/drawer"
import { Field, FieldLabel } from "@/components/ui/field"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Sheet, SheetContent, SheetDescription, SheetFooter, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { useIsMobile } from "@/hooks/use-mobile"
import { cn } from "@/lib/utils"

import { buildReviewActivity } from "./review-activity"
import { ReviewActivityList } from "./review-activity-list"
import { formatReviewDate, formatReviewDuration, pullRequestUrl, shortSha } from "./review-formatters"
import { latestReviewAttempt, projectReviewUsage, reviewJobIsActive, reviewSkipReasonLabel } from "./review-job-model"
import { EnvironmentWarningBadge, ReviewOutcome } from "./review-presentation"
import { ReviewAttemptUsage, ReviewUsageSummary } from "./review-usage-summary"

const HISTORY_PAGE_SIZE = 20

interface ReviewJobDetailsProps {
  projectId: string
  repository: string
  review: PullRequestReviewSummary | null
  onClose(): void
  onRereview(pr: number): void
  pending: boolean
}

export function ReviewJobDetails({ projectId, repository, review, onClose, onRereview, pending }: ReviewJobDetailsProps) {
  const mobile = useIsMobile()
  const [historyPage, setHistoryPage] = useState(1)
  const [selectedJobId, setSelectedJobId] = useState<string | null>(null)
  const history = useQuery(projectPullRequestReviewHistoryQuery(projectId, review?.pr ?? 0, historyPage, HISTORY_PAGE_SIZE))
  const selectedItem = history.data?.items.find((item) => item.job.id === selectedJobId) ?? null
  const detail = useQuery(projectReviewJobQuery(projectId, selectedItem && selectedItem.job.attempt_count > 0 ? selectedItem.job.id : null))

  useEffect(() => {
    setHistoryPage(1)
    setSelectedJobId(null)
  }, [review?.pr])
  useEffect(() => {
    const items = history.data?.items
    const first = items?.[0]
    if (!first) {
      setSelectedJobId(null)
      return
    }
    if (!items.some((item) => item.job.id === selectedJobId)) setSelectedJobId(first.job.id)
  }, [history.data, selectedJobId])

  const selectHistoryPage = (page: number) => {
    setSelectedJobId(null)
    setHistoryPage(page)
  }
  const lifecycleState = review?.lifecycle_state ?? "open"
  const reviewable = lifecycleState === "open"
  const lifecycleDescription = lifecycleState === "merged"
    ? "This PR is merged. Retained Review history remains unchanged."
    : "This PR is currently closed. Retained Review history remains unchanged."
  const content = <>{!reviewable && <Alert className="mx-0 mb-4"><CircleAlert /><AlertTitle>Pull request {lifecycleState}</AlertTitle><AlertDescription>{lifecycleDescription}</AlertDescription></Alert>}<ReviewDetailsContent
    projectId={projectId}
    history={history}
    historyPage={historyPage}
    selectedItem={selectedItem}
    selectedJobId={selectedJobId}
    onSelectJob={setSelectedJobId}
    onSelectHistoryPage={selectHistoryPage}
    detail={detail.data}
    detailLoading={detail.isLoading}
    detailError={detail.error}
    retryDetail={() => void detail.refetch()}
  /></>
  const actions = <ReviewDetailActions pr={review?.pr} repository={repository} reviewable={reviewable} onRereview={onRereview} pending={pending} />
  const description = review ? `${review.history_count} retained history ${review.history_count === 1 ? "record" : "records"}` : ""
  if (mobile) return <Drawer open={Boolean(review)} onOpenChange={(open: boolean) => { if (!open) onClose() }}><DrawerContent className="max-h-[92svh]! overflow-hidden"><DrawerHeader className="shrink-0"><DrawerTitle>Pull request review · PR #{review?.pr ?? "—"}</DrawerTitle><DrawerDescription>{description}</DrawerDescription></DrawerHeader><ScrollArea className="min-h-0 min-w-0 max-w-full flex-1 overflow-hidden px-4 [&_[data-slot=scroll-area-viewport]>div]:!block">{content}</ScrollArea><DrawerFooter className="shrink-0 border-t bg-background">{actions}</DrawerFooter></DrawerContent></Drawer>
  return <Sheet open={Boolean(review)} onOpenChange={(open: boolean) => { if (!open) onClose() }}><SheetContent className="w-full! sm:max-w-2xl!"><SheetHeader><SheetTitle>Pull request review · PR #{review?.pr ?? "—"}</SheetTitle><SheetDescription>{description}</SheetDescription></SheetHeader><ScrollArea className="min-h-0 flex-1 px-4">{content}</ScrollArea><SheetFooter>{actions}</SheetFooter></SheetContent></Sheet>
}

interface ReviewDetailsContentProps {
  projectId: string
  history: UseQueryResult<PullRequestReviewHistoryPage, Error>
  historyPage: number
  selectedItem: PullRequestReviewHistoryItem | null
  selectedJobId: string | null
  onSelectJob(jobId: string): void
  onSelectHistoryPage(page: number): void
  detail?: ReviewJobDetail
  detailLoading: boolean
  detailError: unknown
  retryDetail(): void
}

function ReviewDetailsContent({ projectId, history, historyPage, selectedItem, selectedJobId, onSelectJob, onSelectHistoryPage, detail, detailLoading, detailError, retryDetail }: ReviewDetailsContentProps) {
  if (history.isLoading) return <LoadingState rows={5} />
  if (history.error) return <ErrorState error={history.error} retry={() => void history.refetch()} />
  if (!history.data?.items.length) return <p className="rounded-lg border p-4 text-sm text-muted-foreground">No retained review history.</p>
  return <div className="flex flex-col gap-5 pb-4">
    <Field>
      <FieldLabel htmlFor="review-history">Review history</FieldLabel>
      <Select value={selectedJobId ?? ""} onValueChange={onSelectJob}>
        <SelectTrigger id="review-history" className="w-full"><SelectValue placeholder="Select a review record" /></SelectTrigger>
        <SelectContent><SelectGroup>{history.data.items.map((item) => <SelectItem key={item.job.id} value={item.job.id}>{historyLabel(item)}</SelectItem>)}</SelectGroup></SelectContent>
      </Select>
      <PagePagination page={historyPage} totalPages={history.data.total_pages} onPageChange={onSelectHistoryPage} disabled={history.isFetching} label="Review history pages" />
    </Field>
    {selectedItem && <ReviewRecordContent
      key={selectedItem.job.id}
      projectId={projectId}
      item={selectedItem}
      detail={detail}
      detailLoading={detailLoading}
      detailError={detailError}
      retryDetail={retryDetail}
    />}
  </div>
}

function historyLabel(item: PullRequestReviewHistoryItem) {
  const attempts = `${item.job.attempt_count} ${item.job.attempt_count === 1 ? "attempt" : "attempts"}`
  return `${formatReviewDate(item.job.created_at)} · ${shortSha(item.job.head_sha)} · ${item.job.status} · ${attempts}`
}

function ReviewRecordContent({ projectId, item, detail, detailLoading, detailError, retryDetail }: { projectId: string; item: PullRequestReviewHistoryItem; detail?: ReviewJobDetail; detailLoading: boolean; detailError: unknown; retryDetail(): void }) {
  const [metadataOpen, setMetadataOpen] = useState(false)
  const job = item.job
  const attempted = job.attempt_count > 0
  const attemptsMissing = Boolean(attempted && detail && (detail.attempts?.length ?? 0) === 0)
  const reviewStartedAt = detail?.attempts?.[0]?.started_at
  const usage = projectReviewUsage(detail?.attempts ?? [])
  return <div className="flex flex-col gap-5">
    <div className="flex flex-wrap items-center gap-2"><StatusBadge status={job.status} /><ReviewOutcome job={job} /><EnvironmentWarningBadge job={job} />{job.status === "retry_waiting" && <Badge variant="outline">Retry scheduled</Badge>}</div>
    <dl className="grid grid-cols-2 gap-px overflow-hidden rounded-lg border bg-border sm:grid-cols-4"><Metric label="Attempts" value={`${job.attempt_count}/${job.max_attempts}`} /><Metric label="Created" value={formatReviewDate(job.created_at)} /><Metric label="Duration" value={reviewStartedAt ? formatReviewDuration(reviewStartedAt, job.finished_at) : "—"} /><Metric label="Next attempt" value={job.next_attempt_at ? formatReviewDate(job.next_attempt_at) : "—"} /></dl>
    {attempted && !attemptsMissing && <ReviewUsageSummary usage={usage.total} active={reviewJobIsActive(job)} />}
    {!attempted && <Alert><CircleAlert /><AlertTitle>此记录未启动 Agent 会话</AlertTitle><AlertDescription>{job.skip_reason ? reviewSkipReasonLabel(job.skip_reason) : job.reason || "The review ended before an execution attempt was created."}</AlertDescription></Alert>}
    {attemptsMissing && <Alert variant="destructive"><CircleAlert /><AlertTitle>Review history is incomplete</AlertTitle><AlertDescription>This Job recorded {job.attempt_count} {job.attempt_count === 1 ? "attempt" : "attempts"}, but no retained Attempt data was found.</AlertDescription></Alert>}
    {job.failure && <Alert variant="destructive"><CircleAlert /><AlertTitle>{job.status === "retry_waiting" ? "Attempt failed; retry pending" : "Review failed"}</AlertTitle><AlertDescription><span className="block">{job.failure.message}</span><span className="mt-1 block text-xs opacity-80">{job.failure.category}{job.failure.code ? ` · ${job.failure.code}` : ""}{job.failure.http_status ? ` · HTTP ${job.failure.http_status}` : ""}</span></AlertDescription></Alert>}
    {job.environment_warning && <Alert className="border-amber-500/50 bg-amber-500/5 text-amber-950 dark:text-amber-100"><CircleAlert /><AlertTitle>Latest image refresh failed</AlertTitle><AlertDescription><span className="block">{job.environment_warning.message}</span><span className="mt-1 block text-xs opacity-80">{job.environment_warning.image} · cached {shortSha(job.environment_warning.cached_image_id)} · {formatReviewDate(job.environment_warning.observed_at)}</span></AlertDescription></Alert>}
    {job.submission_intent && !job.submission_receipt && <section className="rounded-lg border bg-muted/35 p-3"><h3 className="text-sm font-medium">GitHub submission pending</h3><p className="mt-1 text-xs text-muted-foreground">The server is reconciling one {job.submission_intent.event.replaceAll("_", " ")} review at head {shortSha(job.submission_intent.head_sha)} with {job.submission_intent.comment_count} inline comments.</p></section>}
    {job.submission_receipt && <section className="rounded-lg border bg-muted/35 p-3"><div className="flex items-center justify-between gap-3"><div><h3 className="text-sm font-medium">GitHub receipt</h3><p className="text-xs text-muted-foreground">Review #{job.submission_receipt.github_review_id} · {formatReviewDate(job.submission_receipt.submitted_at)}</p></div>{job.submission_receipt.html_url && <Button asChild variant="outline" size="sm"><a href={job.submission_receipt.html_url} target="_blank" rel="noreferrer"><ExternalLink /> Open</a></Button>}</div></section>}
    {attempted && (detailLoading ? <LoadingState rows={4} /> : detailError ? <ErrorState error={detailError} retry={retryDetail} /> : detail && !attemptsMissing ? <Attempts projectId={projectId} attempts={detail.attempts} usageByAttemptId={usage.attempts} /> : null)}
    <Collapsible open={metadataOpen} onOpenChange={setMetadataOpen} className="rounded-lg border"><CollapsibleTrigger asChild><Button variant="ghost" className="w-full justify-between rounded-lg px-3" aria-label={`${metadataOpen ? "Hide" : "Show"} technical details`}>Technical details<ChevronDown className={cn("size-4 transition-transform motion-reduce:transition-none", metadataOpen && "rotate-180")} /></Button></CollapsibleTrigger><CollapsibleContent className="border-t"><dl className="divide-y"><DetailRow label="Job" value={job.id} mono /><DetailRow label="Head SHA" value={job.head_sha} mono /><DetailRow label="Reviewer" value={job.reviewer_agent_id || "—"} mono /><DetailRow label="Source" value={job.source} /></dl></CollapsibleContent></Collapsible>
  </div>
}

function Attempts({ projectId, attempts, usageByAttemptId }: { projectId: string; attempts: ReviewRunSummary[]; usageByAttemptId: Record<string, NonNullable<ReviewRunSummary["token_usage"]>> }) {
  const [selected, setSelected] = useState<ReviewRunSummary | null>(() => latestReviewAttempt(attempts))
  const selectedAttempt = attempts.find((attempt) => attempt.id === selected?.id) ?? latestReviewAttempt(attempts)
  return <section className="flex flex-col gap-2.5"><div><h3 className="text-sm font-medium">Attempts</h3><p className="text-xs text-muted-foreground">Each row is one Agent turn; retry-waiting is part of the same logical review.</p></div>{attempts.length === 0 ? <p className="rounded-lg border p-3 text-sm text-muted-foreground">No attempt has started yet.</p> : <div className="flex flex-col gap-2">{attempts.map((attempt) => <button key={attempt.id} type="button" className={cn("flex min-h-11 w-full items-center justify-between gap-3 rounded-lg border p-3 text-left", selectedAttempt?.id === attempt.id && "border-primary/50 bg-muted/40")} onClick={() => setSelected(attempt)}><span className="min-w-0 flex-1"><span className="block text-sm font-medium">Attempt {attempt.attempt_index || 1}</span><span className="block text-xs text-muted-foreground">{formatReviewDate(attempt.started_at)} · {formatReviewDuration(attempt.started_at, attempt.finished_at)}</span><ReviewAttemptUsage usage={usageByAttemptId[attempt.id]} />{attempt.error && <span className="mt-1 block truncate text-xs text-destructive">{attempt.error}</span>}</span><StatusBadge status={attempt.status} /></button>)}</div>}{selectedAttempt?.summary && <section className="rounded-lg border bg-muted/35 p-3"><h4 className="mb-1 text-xs font-medium uppercase tracking-wide text-muted-foreground">Attempt summary</h4><Markdown>{selectedAttempt.summary}</Markdown></section>}<ReviewAttemptActivity projectId={projectId} attempt={selectedAttempt} /></section>
}

function ReviewAttemptActivity({ projectId, attempt }: { projectId: string; attempt: ReviewRunSummary | null }) {
  const detail = useQuery(projectReviewRunQuery(projectId, attempt?.id))
  if (!attempt) return null
  if (detail.isLoading) return <LoadingState rows={3} />
  if (detail.error) return <ErrorState error={detail.error} retry={() => void detail.refetch()} />
  const activity = detail.data ? buildReviewActivity(detail.data as ReviewRunDetail) : null
  return <div className="space-y-2"><h4 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">Attempt activity</h4>{activity && <ReviewActivityList activity={activity} />}</div>
}

function ReviewDetailActions({ pr, repository, reviewable, onRereview, pending }: { pr?: number; repository: string; reviewable: boolean; onRereview(pr: number): void; pending: boolean }) {
  const url = pullRequestUrl(repository, pr)
  return <div className={cn("grid gap-2", reviewable && "grid-cols-2")}>{url ? <Button asChild variant="outline"><a href={url} target="_blank" rel="noreferrer"><ExternalLink data-icon="inline-start" /> Open pull request</a></Button> : <Button variant="outline" disabled>Open pull request</Button>}{reviewable && <Button disabled={!pr || pending} onClick={() => { if (pr) onRereview(pr) }}><RefreshCw data-icon="inline-start" /> Re-review</Button>}</div>
}

function DetailRow({ label, value, mono = false }: { label: string; value: string; mono?: boolean }) {
  return <div className="grid gap-1 px-3 py-2.5 sm:grid-cols-[7rem_1fr]"><dt className="text-xs text-muted-foreground">{label}</dt><dd className={mono ? "break-all font-mono text-xs" : "text-sm"}>{value}</dd></div>
}

function Metric({ label, value }: { label: string; value: string }) {
  return <div className="min-w-0 bg-card p-3"><dt className="text-[11px] text-muted-foreground">{label}</dt><dd className="mt-1 break-words text-sm font-medium tabular-nums">{value}</dd></div>
}
