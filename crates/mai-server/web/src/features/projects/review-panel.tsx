import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { ExternalLink, Eye, MoreHorizontal, Play, RefreshCw } from "lucide-react"
import { useMemo, useState } from "react"
import { toast } from "sonner"

import { api } from "@/api/client"
import type { ProjectDetail, ReviewRunDetail, ReviewRunSummary } from "@/api/product-types"
import { projectReviewRunQuery, projectReviewRunsQuery, queryKeys } from "@/api/queries"
import { EmptyState, ErrorState, LoadingState } from "@/components/page-state"
import { StatusBadge } from "@/components/status"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger } from "@/components/ui/dialog"
import { Drawer, DrawerContent, DrawerDescription, DrawerFooter, DrawerHeader, DrawerTitle } from "@/components/ui/drawer"
import { DropdownMenu, DropdownMenuContent, DropdownMenuGroup, DropdownMenuItem, DropdownMenuTrigger } from "@/components/ui/dropdown-menu"
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Sheet, SheetContent, SheetDescription, SheetFooter, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { useIsMobile } from "@/hooks/use-mobile"

interface ReviewPanelProps {
  project: ProjectDetail
}

interface QueueResponse {
  queued: number[]
  deduped: number[]
  ignored: number[]
}

export function ReviewPanel({ project }: ReviewPanelProps) {
  const queryClient = useQueryClient()
  const runs = useQuery(projectReviewRunsQuery(project.id))
  const [selectedRun, setSelectedRun] = useState<ReviewRunSummary | null>(null)
  const [runDialogOpen, setRunDialogOpen] = useState(false)
  const [pr, setPr] = useState("")
  const queue = useMutation({
    mutationFn: (number: number) => api<QueueResponse>(`/projects/${project.id}/pull-requests/${number}/review`, { method: "POST" }),
    onSuccess: async (response, number) => {
      if (response.queued.includes(number)) toast.success(`Review queued for PR #${number}`)
      else if (response.deduped.includes(number)) toast.info(`PR #${number} is already queued or running`)
      else toast.info(`PR #${number} was not queued`)
      setPr("")
      setRunDialogOpen(false)
      await queryClient.invalidateQueries({ queryKey: queryKeys.projectReviewRuns(project.id) })
    },
    onError: (error) => toast.error(error.message),
  })
  const summary = useMemo(() => summarizeRuns(runs.data?.runs ?? []), [runs.data])
  const repository = String(project.repository_full_name || project.repository || "")

  return (
    <div className="min-h-0 flex-1 overflow-auto">
      <div className="mx-auto flex max-w-6xl flex-col gap-5 p-4 md:p-6">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div>
            <h2 className="text-base font-semibold">Pull request reviews</h2>
            <p className="text-sm text-muted-foreground">Run and inspect agent reviews for this project.</p>
          </div>
          <Dialog open={runDialogOpen} onOpenChange={setRunDialogOpen}>
            <DialogTrigger asChild><Button><Play data-icon="inline-start" /> Run review</Button></DialogTrigger>
            <DialogContent>
              <DialogHeader><DialogTitle>Run pull request review</DialogTitle><DialogDescription>Queue a manual review even when automatic review is disabled.</DialogDescription></DialogHeader>
              <FieldGroup>
                <Field>
                  <FieldLabel htmlFor="review-pr">Pull request number</FieldLabel>
                  <Input id="review-pr" inputMode="numeric" value={pr} onChange={(event) => setPr(event.target.value.replace(/\D/g, ""))} placeholder="1631" />
                  <FieldDescription>The latest head SHA is resolved by the server before review starts.</FieldDescription>
                </Field>
              </FieldGroup>
              <DialogFooter><Button variant="outline" onClick={() => setRunDialogOpen(false)}>Cancel</Button><Button disabled={!pr || queue.isPending} onClick={() => queue.mutate(Number(pr))}><Play data-icon="inline-start" /> Queue review</Button></DialogFooter>
            </DialogContent>
          </Dialog>
        </div>

        <div className="flex flex-wrap gap-2" aria-label="Review run summary">
          <Badge variant="secondary">{summary.running} running</Badge>
          <Badge variant="outline">{summary.completed} completed</Badge>
          <Badge variant={summary.failed ? "destructive" : "outline"}>{summary.failed} failed</Badge>
        </div>

        {runs.isLoading && <LoadingState rows={5} />}
        {runs.error && <ErrorState error={runs.error} retry={() => void runs.refetch()} />}
        {runs.data && runs.data.runs.length === 0 && <EmptyState title="No review runs yet" description="Queue a pull request review to see its status and result here." action={<Button onClick={() => setRunDialogOpen(true)}><Play data-icon="inline-start" /> Run review</Button>} />}
        {runs.data && runs.data.runs.length > 0 && <>
          <ReviewTable runs={runs.data.runs} repository={repository} onDetails={setSelectedRun} onRereview={(number) => queue.mutate(number)} pending={queue.isPending} />
          <ReviewMobileList runs={runs.data.runs} repository={repository} onDetails={setSelectedRun} onRereview={(number) => queue.mutate(number)} pending={queue.isPending} />
        </>}
      </div>
      <ReviewDetailsOverlay projectId={project.id} repository={repository} run={selectedRun} onClose={() => setSelectedRun(null)} onRereview={(number) => queue.mutate(number)} pending={queue.isPending} />
    </div>
  )
}

function ReviewTable({ runs, repository, onDetails, onRereview, pending }: ReviewRunListProps) {
  return (
    <div className="hidden overflow-hidden rounded-lg border md:block">
      <Table>
        <TableHeader><TableRow><TableHead>Status</TableHead><TableHead>Pull request</TableHead><TableHead>Result</TableHead><TableHead>Started</TableHead><TableHead>Duration</TableHead><TableHead>Usage</TableHead><TableHead className="w-24 text-right">Actions</TableHead></TableRow></TableHeader>
        <TableBody>{runs.map((run) => <TableRow key={run.id} className="cursor-pointer" onClick={() => onDetails(run)}>
          <TableCell><StatusBadge status={run.status} /></TableCell>
          <TableCell className="font-medium">PR #{run.pr ?? "—"}</TableCell>
          <TableCell><ReviewOutcome run={run} /></TableCell>
          <TableCell className="text-muted-foreground">{formatDate(run.started_at)}</TableCell>
          <TableCell className="text-muted-foreground">{formatDuration(run.started_at, run.finished_at)}</TableCell>
          <TableCell className="text-muted-foreground">{formatUsage(run)}</TableCell>
          <TableCell onClick={(event) => event.stopPropagation()}><ReviewActions run={run} repository={repository} onDetails={onDetails} onRereview={onRereview} pending={pending} /></TableCell>
        </TableRow>)}</TableBody>
      </Table>
    </div>
  )
}

function ReviewMobileList({ runs, repository, onDetails, onRereview, pending }: ReviewRunListProps) {
  return <div className="divide-y overflow-hidden rounded-lg border md:hidden">{runs.map((run) => <div key={run.id} className="flex items-center gap-3 p-3">
    <StatusBadge status={run.status} />
    <button type="button" className="min-w-0 flex-1 text-left" onClick={() => onDetails(run)}>
      <span className="flex flex-wrap items-center gap-2"><span className="font-medium">PR #{run.pr ?? "—"}</span><ReviewOutcome run={run} /></span>
      <span className="block truncate text-xs text-muted-foreground">{run.error || formatDate(run.started_at)} · {formatDuration(run.started_at, run.finished_at)}</span>
    </button>
    <ReviewActions run={run} repository={repository} onDetails={onDetails} onRereview={onRereview} pending={pending} />
  </div>)}</div>
}

interface ReviewRunListProps {
  runs: ReviewRunSummary[]
  repository: string
  onDetails(run: ReviewRunSummary): void
  onRereview(pr: number): void
  pending: boolean
}

function ReviewActions({ run, repository, onDetails, onRereview, pending }: Omit<ReviewRunListProps, "runs"> & { run: ReviewRunSummary }) {
  const url = pullRequestUrl(repository, run.pr)
  return <div className="flex justify-end gap-1">
    {url && <Button asChild variant="ghost" size="icon" aria-label={`Open PR #${run.pr}`}><a href={url} target="_blank" rel="noreferrer"><ExternalLink data-icon="inline-start" /></a></Button>}
    <DropdownMenu>
      <DropdownMenuTrigger asChild><Button variant="ghost" size="icon" aria-label={`Actions for PR #${run.pr ?? "unknown"}`}><MoreHorizontal data-icon="inline-start" /></Button></DropdownMenuTrigger>
      <DropdownMenuContent align="end"><DropdownMenuGroup>
        <DropdownMenuItem onSelect={() => onDetails(run)}><Eye /> View details</DropdownMenuItem>
        <DropdownMenuItem disabled={!run.pr || pending} onSelect={() => { if (run.pr) onRereview(run.pr) }}><RefreshCw /> Re-review</DropdownMenuItem>
      </DropdownMenuGroup></DropdownMenuContent>
    </DropdownMenu>
  </div>
}

function ReviewDetailsOverlay({ projectId, repository, run, onClose, onRereview, pending }: {
  projectId: string
  repository: string
  run: ReviewRunSummary | null
  onClose(): void
  onRereview(pr: number): void
  pending: boolean
}) {
  const mobile = useIsMobile()
  const detail = useQuery(projectReviewRunQuery(projectId, run?.id))
  const content = <ReviewDetailContent run={run} detail={detail.data} loading={detail.isLoading} error={detail.error} retry={() => void detail.refetch()} />
  const actions = <ReviewDetailActions run={run} repository={repository} onRereview={onRereview} pending={pending} />
  if (mobile) return <Drawer open={Boolean(run)} onOpenChange={(open: boolean) => { if (!open) onClose() }}><DrawerContent className="max-h-[92svh]! overflow-hidden"><DrawerHeader className="shrink-0"><DrawerTitle>Review run · PR #{run?.pr ?? "—"}</DrawerTitle><DrawerDescription>{run ? formatDate(run.started_at) : ""}</DrawerDescription></DrawerHeader><ScrollArea className="min-h-0 flex-1 overflow-hidden px-4">{content}</ScrollArea><DrawerFooter className="shrink-0 border-t bg-background">{actions}</DrawerFooter></DrawerContent></Drawer>
  return <Sheet open={Boolean(run)} onOpenChange={(open: boolean) => { if (!open) onClose() }}><SheetContent className="w-full sm:max-w-xl"><SheetHeader><SheetTitle>Review run · PR #{run?.pr ?? "—"}</SheetTitle><SheetDescription>{run ? formatDate(run.started_at) : ""}</SheetDescription></SheetHeader><ScrollArea className="min-h-0 flex-1 px-4">{content}</ScrollArea><SheetFooter>{actions}</SheetFooter></SheetContent></Sheet>
}

function ReviewDetailContent({ run, detail, loading, error, retry }: { run: ReviewRunSummary | null; detail?: ReviewRunDetail; loading: boolean; error: unknown; retry(): void }) {
  if (!run) return null
  if (loading) return <LoadingState rows={5} />
  if (error) return <ErrorState error={error} retry={retry} />
  const value = detail ?? run
  return <div className="flex flex-col gap-5 pb-4">
    <div className="flex items-center gap-2"><StatusBadge status={value.status} /><ReviewOutcome run={value} /></div>
    <dl className="divide-y rounded-lg border">
      <DetailRow label="Started" value={formatDate(value.started_at)} />
      <DetailRow label="Duration" value={formatDuration(value.started_at, value.finished_at)} />
      <DetailRow label="Reviewer" value={value.reviewer_agent_id || "—"} mono />
      <DetailRow label="Turn" value={value.turn_id || "—"} mono />
      <DetailRow label="Tokens" value={(value.token_usage?.total_tokens ?? 0).toLocaleString()} />
      <DetailRow label="Cached input" value={(value.token_usage?.cached_input_tokens ?? 0).toLocaleString()} />
    </dl>
    {(value.summary || value.error) && <section className="flex flex-col gap-2"><h3 className="text-sm font-medium">Summary</h3><p className="whitespace-pre-wrap rounded-lg border bg-muted/40 p-3 text-sm">{value.error || value.summary}</p></section>}
    {detail && detail.messages.length > 0 && <section className="flex flex-col gap-2"><h3 className="text-sm font-medium">Messages</h3><div className="divide-y rounded-lg border">{detail.messages.map((message, index) => <div key={`${message.created_at}:${index}`} className="grid gap-1 p-3 sm:grid-cols-[5rem_1fr]"><span className="text-xs capitalize text-muted-foreground">{message.role}</span><p className="whitespace-pre-wrap text-sm">{message.content}</p></div>)}</div></section>}
    {detail && detail.events.length > 0 && <section className="flex flex-col gap-2"><h3 className="text-sm font-medium">Session events</h3><div className="max-h-64 divide-y overflow-auto rounded-lg border">{detail.events.map((event) => <div key={event.eventId} className="flex items-center gap-3 px-3 py-2 text-xs"><code className="min-w-0 flex-1 truncate">{event.kind.type}</code><span className="text-muted-foreground">{event.position.persistence === "durable" ? `#${event.position.sequence}` : `r${event.position.revision}`}</span></div>)}</div></section>}
  </div>
}

function ReviewDetailActions({ run, repository, onRereview, pending }: { run: ReviewRunSummary | null; repository: string; onRereview(pr: number): void; pending: boolean }) {
  const url = pullRequestUrl(repository, run?.pr)
  return <div className="grid grid-cols-2 gap-2">{url ? <Button asChild variant="outline"><a href={url} target="_blank" rel="noreferrer"><ExternalLink data-icon="inline-start" /> Open pull request</a></Button> : <Button variant="outline" disabled>Open pull request</Button>}<Button disabled={!run?.pr || pending} onClick={() => { if (run?.pr) onRereview(run.pr) }}><RefreshCw data-icon="inline-start" /> Re-review</Button></div>
}

function ReviewOutcome({ run }: { run: ReviewRunSummary }) {
  const result = reviewResult(run)
  if (!result) return <span className="text-sm text-muted-foreground">—</span>
  return <Badge variant={result.variant}>{result.label}</Badge>
}

function reviewResult(run: ReviewRunSummary) {
  switch (run.review_event) {
    case "approve":
      return { label: "Approved", variant: "secondary" as const }
    case "request_changes":
      return { label: "Request changes", variant: "destructive" as const }
    case "comment":
      return { label: "Commented", variant: "outline" as const }
    case null:
    case undefined:
      break
  }

  switch (run.outcome) {
    case "failed":
      return { label: "Failed", variant: "destructive" as const }
    case "no_eligible_pr":
      return { label: "No eligible pull request", variant: "outline" as const }
    case "review_submitted":
      return { label: "Review submitted", variant: "outline" as const }
    case null:
    case undefined:
      return null
  }
}

function DetailRow({ label, value, mono = false }: { label: string; value: string; mono?: boolean }) {
  return <div className="grid gap-1 px-3 py-2.5 sm:grid-cols-[7rem_1fr]"><dt className="text-xs text-muted-foreground">{label}</dt><dd className={mono ? "break-all font-mono text-xs" : "text-sm"}>{value}</dd></div>
}

function summarizeRuns(runs: ReviewRunSummary[]) {
  return runs.reduce((summary, run) => {
    if (run.status === "running" || run.status === "queued") summary.running += 1
    else if (run.status === "completed") summary.completed += 1
    else summary.failed += 1
    return summary
  }, { running: 0, completed: 0, failed: 0 })
}

function pullRequestUrl(repository: string, pr?: number | null) {
  return repository && pr ? `https://github.com/${repository}/pull/${pr}` : null
}

function formatDate(value: string) {
  const date = new Date(value)
  return Number.isNaN(date.getTime()) ? value : new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(date)
}

function formatDuration(startedAt: string, finishedAt?: string | null) {
  const start = new Date(startedAt).getTime()
  const end = finishedAt ? new Date(finishedAt).getTime() : Date.now()
  if (!Number.isFinite(start) || !Number.isFinite(end) || end < start) return "—"
  const seconds = Math.floor((end - start) / 1000)
  if (seconds < 60) return `${seconds}s`
  const minutes = Math.floor(seconds / 60)
  return minutes < 60 ? `${minutes}m ${seconds % 60}s` : `${Math.floor(minutes / 60)}h ${minutes % 60}m`
}

function formatUsage(run: ReviewRunSummary) {
  const usage = run.token_usage
  if (!usage?.total_tokens) return "—"
  const cache = usage.input_tokens > 0 ? Math.round((usage.cached_input_tokens / usage.input_tokens) * 100) : 0
  return `${usage.total_tokens.toLocaleString()} · ${cache}% cached`
}
