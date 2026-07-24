import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { ChevronDown, CircleAlert, ExternalLink, Eye, MoreHorizontal, Play, RefreshCw } from "lucide-react"
import { useMemo, useState } from "react"
import { toast } from "sonner"

import { api } from "@/api/client"
import type { ProjectDetail, ReviewJobDetail, ReviewJobSummary, ReviewRunDetail, ReviewRunSummary } from "@/api/product-types"
import { projectReviewJobQuery, projectReviewJobsQuery, projectReviewRunQuery, queryKeys } from "@/api/queries"
import { Markdown } from "@/components/markdown"
import { EmptyState, ErrorState, LoadingState } from "@/components/page-state"
import { StatusBadge } from "@/components/status"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger } from "@/components/ui/dialog"
import { Drawer, DrawerContent, DrawerDescription, DrawerFooter, DrawerHeader, DrawerTitle } from "@/components/ui/drawer"
import { DropdownMenu, DropdownMenuContent, DropdownMenuGroup, DropdownMenuItem, DropdownMenuTrigger } from "@/components/ui/dropdown-menu"
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Sheet, SheetContent, SheetDescription, SheetFooter, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { useIsMobile } from "@/hooks/use-mobile"
import { cn } from "@/lib/utils"

import { buildReviewActivity } from "./review-activity"
import { ReviewActivityList } from "./review-activity-list"
import { latestReviewAttempt, projectReviewUsage, reviewJobIsActive, reviewSkipReasonLabel, summarizeReviewJobs } from "./review-job-model"
import { ReviewAttemptUsage, ReviewUsageSummary } from "./review-usage-summary"

interface QueueResponse {
  queued: number[]
  deduped: number[]
  ignored: number[]
  jobs: ReviewJobSummary[]
}

export function ReviewPanel({ project }: { project: ProjectDetail }) {
  const queryClient = useQueryClient()
  const jobs = useQuery(projectReviewJobsQuery(project.id))
  const [selectedJob, setSelectedJob] = useState<ReviewJobSummary | null>(null)
  const [runDialogOpen, setRunDialogOpen] = useState(false)
  const [pr, setPr] = useState("")
  const queue = useMutation({
    mutationFn: (number: number) => api<QueueResponse>(`/projects/${project.id}/pull-requests/${number}/review`, { method: "POST" }),
    onSuccess: async (response, number) => {
      if (response.queued.includes(number)) toast.success(`Review queued for PR #${number}`)
      else if (response.deduped.includes(number)) toast.info(`PR #${number} already has an active review job`)
      else toast.info(`PR #${number} was not queued`)
      setPr("")
      setRunDialogOpen(false)
      await queryClient.invalidateQueries({ queryKey: queryKeys.projectReviewJobs(project.id) })
    },
    onError: (error) => toast.error(error.message),
  })
  const summary = useMemo(() => summarizeReviewJobs(jobs.data?.jobs ?? []), [jobs.data])
  const repository = String(project.repository_full_name || project.repository || "")

  return <div className="min-h-0 flex-1 overflow-auto">
    <div className="mx-auto flex max-w-6xl flex-col gap-5 p-4 md:p-6">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div><h2 className="text-base font-semibold">Pull request reviews</h2><p className="text-sm text-muted-foreground">Logical review jobs and their retry attempts.</p></div>
        <Dialog open={runDialogOpen} onOpenChange={setRunDialogOpen}>
          <DialogTrigger asChild><Button><Play data-icon="inline-start" /> Run review</Button></DialogTrigger>
          <DialogContent>
            <DialogHeader><DialogTitle>Run pull request review</DialogTitle><DialogDescription>Queue a manual review even when automatic review is disabled.</DialogDescription></DialogHeader>
            <FieldGroup><Field><FieldLabel htmlFor="review-pr">Pull request number</FieldLabel><Input id="review-pr" inputMode="numeric" value={pr} onChange={(event) => setPr(event.target.value.replace(/\D/g, ""))} placeholder="1631" /><FieldDescription>The server fixes the logical job to the current head SHA.</FieldDescription></Field></FieldGroup>
            <DialogFooter><Button variant="outline" onClick={() => setRunDialogOpen(false)}>Cancel</Button><Button disabled={!pr || queue.isPending} onClick={() => queue.mutate(Number(pr))}><Play data-icon="inline-start" /> Queue review</Button></DialogFooter>
          </DialogContent>
        </Dialog>
      </div>
      <div className="flex flex-wrap gap-2" aria-label="Review job summary"><Badge variant="secondary">{summary.active} active</Badge><Badge variant="outline">{summary.succeeded} succeeded</Badge><Badge variant="outline">{summary.skipped} skipped</Badge><Badge variant={summary.failed ? "destructive" : "outline"}>{summary.failed} failed</Badge></div>
      {jobs.isLoading && <LoadingState rows={5} />}
      {jobs.error && <ErrorState error={jobs.error} retry={() => void jobs.refetch()} />}
      {jobs.data?.jobs.length === 0 && <EmptyState title="No review jobs yet" description="Queue a pull request review to see its lifecycle and attempts here." action={<Button onClick={() => setRunDialogOpen(true)}><Play data-icon="inline-start" /> Run review</Button>} />}
      {jobs.data && jobs.data.jobs.length > 0 && <><ReviewTable jobs={jobs.data.jobs} repository={repository} onDetails={setSelectedJob} onRereview={(number) => queue.mutate(number)} pending={queue.isPending} /><ReviewMobileList jobs={jobs.data.jobs} repository={repository} onDetails={setSelectedJob} onRereview={(number) => queue.mutate(number)} pending={queue.isPending} /></>}
    </div>
    <ReviewDetailsOverlay projectId={project.id} repository={repository} job={selectedJob} onClose={() => setSelectedJob(null)} onRereview={(number) => queue.mutate(number)} pending={queue.isPending} />
  </div>
}

interface ReviewJobListProps {
  jobs: ReviewJobSummary[]
  repository: string
  onDetails(job: ReviewJobSummary): void
  onRereview(pr: number): void
  pending: boolean
}

function ReviewTable({ jobs, repository, onDetails, onRereview, pending }: ReviewJobListProps) {
  return <div className="hidden overflow-hidden rounded-lg border lg:block"><Table><TableHeader><TableRow><TableHead>Status</TableHead><TableHead>Pull request</TableHead><TableHead>Result</TableHead><TableHead>Attempts</TableHead><TableHead>Created</TableHead><TableHead>Next action</TableHead><TableHead className="w-24 text-right">Actions</TableHead></TableRow></TableHeader><TableBody>{jobs.map((job) => <TableRow key={job.id} className="cursor-pointer" onClick={() => onDetails(job)}><TableCell><StatusBadge status={job.status} /></TableCell><TableCell className="font-medium">PR #{job.pr}</TableCell><TableCell><ReviewOutcome job={job} /></TableCell><TableCell className="text-muted-foreground tabular-nums">{job.attempt_count}/{job.max_attempts}</TableCell><TableCell className="text-muted-foreground">{formatDate(job.created_at)}</TableCell><TableCell className="text-muted-foreground">{job.next_attempt_at ? formatDate(job.next_attempt_at) : "—"}</TableCell><TableCell onClick={(event) => event.stopPropagation()}><ReviewActions job={job} repository={repository} onDetails={onDetails} onRereview={onRereview} pending={pending} /></TableCell></TableRow>)}</TableBody></Table></div>
}

function ReviewMobileList({ jobs, repository, onDetails, onRereview, pending }: ReviewJobListProps) {
  return <div className="divide-y overflow-hidden rounded-lg border lg:hidden">{jobs.map((job) => <div key={job.id} className="flex items-center gap-3 p-3"><StatusBadge status={job.status} /><button type="button" className="min-w-0 flex-1 text-left" onClick={() => onDetails(job)}><span className="flex flex-wrap items-center gap-2"><span className="font-medium">PR #{job.pr}</span><ReviewOutcome job={job} /></span><span className="block truncate text-xs text-muted-foreground">{job.failure?.message || (job.status === "skipped" ? reviewSkipReasonLabel(job.skip_reason) : `${job.attempt_count}/${job.max_attempts} attempts · ${formatDate(job.created_at)}`)}</span></button><ReviewActions job={job} repository={repository} onDetails={onDetails} onRereview={onRereview} pending={pending} /></div>)}</div>
}

function ReviewActions({ job, repository, onDetails, onRereview, pending }: Omit<ReviewJobListProps, "jobs"> & { job: ReviewJobSummary }) {
  const url = pullRequestUrl(repository, job.pr)
  return <div className="flex justify-end gap-1">{url && <Button asChild variant="ghost" size="icon" aria-label={`Open PR #${job.pr}`}><a href={url} target="_blank" rel="noreferrer"><ExternalLink data-icon="inline-start" /></a></Button>}<DropdownMenu><DropdownMenuTrigger asChild><Button variant="ghost" size="icon" aria-label={`Actions for PR #${job.pr}`}><MoreHorizontal data-icon="inline-start" /></Button></DropdownMenuTrigger><DropdownMenuContent align="end"><DropdownMenuGroup><DropdownMenuItem onSelect={() => onDetails(job)}><Eye /> View details</DropdownMenuItem><DropdownMenuItem disabled={pending} onSelect={() => onRereview(job.pr)}><RefreshCw /> Re-review</DropdownMenuItem></DropdownMenuGroup></DropdownMenuContent></DropdownMenu></div>
}

function ReviewDetailsOverlay({ projectId, repository, job, onClose, onRereview, pending }: { projectId: string; repository: string; job: ReviewJobSummary | null; onClose(): void; onRereview(pr: number): void; pending: boolean }) {
  const mobile = useIsMobile()
  const detail = useQuery(projectReviewJobQuery(projectId, job?.id))
  const content = <ReviewDetailContent projectId={projectId} job={job} detail={detail.data} loading={detail.isLoading} error={detail.error} retry={() => void detail.refetch()} />
  const actions = <ReviewDetailActions job={job} repository={repository} onRereview={onRereview} pending={pending} />
  if (mobile) return <Drawer open={Boolean(job)} onOpenChange={(open: boolean) => { if (!open) onClose() }}><DrawerContent className="max-h-[92svh]! overflow-hidden"><DrawerHeader className="shrink-0"><DrawerTitle>Review job · PR #{job?.pr ?? "—"}</DrawerTitle><DrawerDescription>{job ? formatDate(job.created_at) : ""}</DrawerDescription></DrawerHeader><ScrollArea className="min-h-0 min-w-0 max-w-full flex-1 overflow-hidden px-4 [&_[data-slot=scroll-area-viewport]>div]:!block">{content}</ScrollArea><DrawerFooter className="shrink-0 border-t bg-background">{actions}</DrawerFooter></DrawerContent></Drawer>
  return <Sheet open={Boolean(job)} onOpenChange={(open: boolean) => { if (!open) onClose() }}><SheetContent className="w-full! sm:max-w-2xl!"><SheetHeader><SheetTitle>Review job · PR #{job?.pr ?? "—"}</SheetTitle><SheetDescription>{job ? formatDate(job.created_at) : ""}</SheetDescription></SheetHeader><ScrollArea className="min-h-0 flex-1 px-4">{content}</ScrollArea><SheetFooter>{actions}</SheetFooter></SheetContent></Sheet>
}

function ReviewDetailContent({ projectId, job, detail, loading, error, retry }: { projectId: string; job: ReviewJobSummary | null; detail?: ReviewJobDetail; loading: boolean; error: unknown; retry(): void }) {
  const [metadataOpen, setMetadataOpen] = useState(false)
  if (!job) return null
  if (loading) return <LoadingState rows={5} />
  if (error) return <ErrorState error={error} retry={retry} />
  const value = detail ?? job
  const usage = projectReviewUsage(detail?.attempts ?? [])
  return <div className="flex flex-col gap-5 pb-4">
    <div className="flex flex-wrap items-center gap-2"><StatusBadge status={value.status} /><ReviewOutcome job={value} />{value.status === "retry_waiting" && <Badge variant="outline">Retry scheduled</Badge>}</div>
    <dl className="grid grid-cols-2 gap-px overflow-hidden rounded-lg border bg-border sm:grid-cols-4"><Metric label="Attempts" value={`${value.attempt_count}/${value.max_attempts}`} /><Metric label="Created" value={formatDate(value.created_at)} /><Metric label="Duration" value={formatDuration(value.created_at, value.finished_at)} /><Metric label="Next attempt" value={value.next_attempt_at ? formatDate(value.next_attempt_at) : "—"} /></dl>
    <ReviewUsageSummary usage={usage.total} active={reviewJobIsActive(value)} />
    {value.status === "skipped" && <Alert><CircleAlert /><AlertTitle>Review skipped before an Agent was created</AlertTitle><AlertDescription>{reviewSkipReasonLabel(value.skip_reason)}</AlertDescription></Alert>}
    {value.failure && <Alert variant="destructive"><CircleAlert /><AlertTitle>{value.status === "retry_waiting" ? "Attempt failed; retry pending" : "Review failed"}</AlertTitle><AlertDescription><span className="block">{value.failure.message}</span><span className="mt-1 block text-xs opacity-80">{value.failure.category}{value.failure.code ? ` · ${value.failure.code}` : ""}{value.failure.http_status ? ` · HTTP ${value.failure.http_status}` : ""}</span></AlertDescription></Alert>}
    {value.submission_intent && !value.submission_receipt && <section className="rounded-lg border bg-muted/35 p-3"><h3 className="text-sm font-medium">GitHub submission pending</h3><p className="mt-1 text-xs text-muted-foreground">The server is reconciling one {value.submission_intent.event.replaceAll("_", " ")} review at head {shortSha(value.submission_intent.head_sha)} with {value.submission_intent.comment_count} inline comments.</p></section>}
    {value.submission_receipt && <section className="rounded-lg border bg-muted/35 p-3"><div className="flex items-center justify-between gap-3"><div><h3 className="text-sm font-medium">GitHub receipt</h3><p className="text-xs text-muted-foreground">Review #{value.submission_receipt.github_review_id} · {formatDate(value.submission_receipt.submitted_at)}</p></div>{value.submission_receipt.html_url && <Button asChild variant="outline" size="sm"><a href={value.submission_receipt.html_url} target="_blank" rel="noreferrer"><ExternalLink /> Open</a></Button>}</div></section>}
    {detail && <Attempts projectId={projectId} attempts={detail.attempts} usageByAttemptId={usage.attempts} />}
    <Collapsible open={metadataOpen} onOpenChange={setMetadataOpen} className="rounded-lg border"><CollapsibleTrigger asChild><Button variant="ghost" className="w-full justify-between rounded-lg px-3" aria-label={`${metadataOpen ? "Hide" : "Show"} technical details`}>Technical details<ChevronDown className={cn("size-4 transition-transform motion-reduce:transition-none", metadataOpen && "rotate-180")} /></Button></CollapsibleTrigger><CollapsibleContent className="border-t"><dl className="divide-y"><DetailRow label="Job" value={value.id} mono /><DetailRow label="Head SHA" value={value.head_sha} mono /><DetailRow label="Reviewer" value={value.reviewer_agent_id || "—"} mono /><DetailRow label="Source" value={value.source} /></dl></CollapsibleContent></Collapsible>
  </div>
}

function Attempts({ projectId, attempts, usageByAttemptId }: { projectId: string; attempts: ReviewRunSummary[]; usageByAttemptId: Record<string, NonNullable<ReviewRunSummary["token_usage"]>> }) {
  const [selected, setSelected] = useState<ReviewRunSummary | null>(() => latestReviewAttempt(attempts))
  const selectedAttempt = attempts.find((attempt) => attempt.id === selected?.id) ?? latestReviewAttempt(attempts)
  return <section className="flex flex-col gap-2.5"><div><h3 className="text-sm font-medium">Attempts</h3><p className="text-xs text-muted-foreground">Each row is one Agent turn; retry-waiting is part of the same logical review.</p></div>{attempts.length === 0 ? <p className="rounded-lg border p-3 text-sm text-muted-foreground">No attempt has started yet.</p> : <div className="flex flex-col gap-2">{attempts.map((attempt) => <button key={attempt.id} type="button" className={cn("flex min-h-11 w-full items-center justify-between gap-3 rounded-lg border p-3 text-left", selectedAttempt?.id === attempt.id && "border-primary/50 bg-muted/40")} onClick={() => setSelected(attempt)}><span className="min-w-0 flex-1"><span className="block text-sm font-medium">Attempt {attempt.attempt_index || 1}</span><span className="block text-xs text-muted-foreground">{formatDate(attempt.started_at)} · {formatDuration(attempt.started_at, attempt.finished_at)}</span><ReviewAttemptUsage usage={usageByAttemptId[attempt.id]} />{attempt.error && <span className="mt-1 block truncate text-xs text-destructive">{attempt.error}</span>}</span><StatusBadge status={attempt.status} /></button>)}</div>}{selectedAttempt?.summary && <section className="rounded-lg border bg-muted/35 p-3"><h4 className="mb-1 text-xs font-medium uppercase tracking-wide text-muted-foreground">Attempt summary</h4><Markdown>{selectedAttempt.summary}</Markdown></section>}<ReviewAttemptActivity projectId={projectId} attempt={selectedAttempt} /></section>
}

function ReviewAttemptActivity({ projectId, attempt }: { projectId: string; attempt: ReviewRunSummary | null }) {
  const detail = useQuery(projectReviewRunQuery(projectId, attempt?.id))
  if (!attempt) return null
  if (detail.isLoading) return <LoadingState rows={3} />
  if (detail.error) return <ErrorState error={detail.error} retry={() => void detail.refetch()} />
  const activity = detail.data ? buildReviewActivity(detail.data as ReviewRunDetail) : []
  return <div className="space-y-2"><h4 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">Attempt activity</h4><ReviewActivityList activity={activity} /></div>
}

function ReviewDetailActions({ job, repository, onRereview, pending }: { job: ReviewJobSummary | null; repository: string; onRereview(pr: number): void; pending: boolean }) {
  const url = pullRequestUrl(repository, job?.pr)
  return <div className="grid grid-cols-2 gap-2">{url ? <Button asChild variant="outline"><a href={url} target="_blank" rel="noreferrer"><ExternalLink data-icon="inline-start" /> Open pull request</a></Button> : <Button variant="outline" disabled>Open pull request</Button>}<Button disabled={!job?.pr || pending} onClick={() => { if (job?.pr) onRereview(job.pr) }}><RefreshCw data-icon="inline-start" /> Re-review</Button></div>
}

function ReviewOutcome({ job }: { job: ReviewJobSummary }) {
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

function DetailRow({ label, value, mono = false }: { label: string; value: string; mono?: boolean }) { return <div className="grid gap-1 px-3 py-2.5 sm:grid-cols-[7rem_1fr]"><dt className="text-xs text-muted-foreground">{label}</dt><dd className={mono ? "break-all font-mono text-xs" : "text-sm"}>{value}</dd></div> }
function Metric({ label, value }: { label: string; value: string }) { return <div className="min-w-0 bg-card p-3"><dt className="text-[11px] text-muted-foreground">{label}</dt><dd className="mt-1 break-words text-sm font-medium tabular-nums">{value}</dd></div> }
function pullRequestUrl(repository: string, pr?: number | null) { return repository && pr ? `https://github.com/${repository}/pull/${pr}` : null }
function shortSha(value: string) { return value.slice(0, 8) }
function formatDate(value: string) { const date = new Date(value); return Number.isNaN(date.getTime()) ? value : new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(date) }
function formatDuration(startedAt: string, finishedAt?: string | null) { const start = new Date(startedAt).getTime(); const end = finishedAt ? new Date(finishedAt).getTime() : Date.now(); if (!Number.isFinite(start) || !Number.isFinite(end) || end < start) return "—"; const seconds = Math.floor((end - start) / 1000); if (seconds < 60) return `${seconds}s`; const minutes = Math.floor(seconds / 60); return minutes < 60 ? `${minutes}m ${seconds % 60}s` : `${Math.floor(minutes / 60)}h ${minutes % 60}m` }
