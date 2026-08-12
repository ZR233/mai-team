import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { Play } from "lucide-react"
import { useCallback, useEffect, useRef, useState } from "react"
import { toast } from "sonner"

import { api } from "@/api/client"
import type { ProjectDetail, PullRequestStateRefreshSummary, PullRequestReviewSummary, ReviewJobSummary } from "@/api/product-types"
import { projectPullRequestReviewsQuery, queryKeys } from "@/api/queries"
import { PagePagination } from "@/components/page-pagination"
import { EmptyState, ErrorState, LoadingState } from "@/components/page-state"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger } from "@/components/ui/dialog"
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"

import { ReviewJobDetails } from "./review-job-details"
import { ReviewJobList } from "./review-job-list"

const REVIEW_PAGE_SIZE = 20

interface QueueResponse {
  queued: number[]
  deduped: number[]
  ignored: number[]
  jobs: ReviewJobSummary[]
}

interface ReviewPanelProps {
  project: ProjectDetail
  page: number
  onPageChange(page: number): void
}

export function ReviewPanel({ project, page, onPageChange }: ReviewPanelProps) {
  const queryClient = useQueryClient()
  const reviews = useQuery({
    ...projectPullRequestReviewsQuery(project.id, page, REVIEW_PAGE_SIZE),
    placeholderData: keepPreviousData,
  })
  const [selectedReview, setSelectedReview] = useState<PullRequestReviewSummary | null>(null)
  const [runDialogOpen, setRunDialogOpen] = useState(false)
  const [pr, setPr] = useState("")
  const refreshedProject = useRef<string | null>(null)
  const changePage = useCallback((nextPage: number) => {
    setSelectedReview(null)
    onPageChange(nextPage)
  }, [onPageChange])
  const queue = useMutation({
    mutationFn: (number: number) => api<QueueResponse>(`/projects/${project.id}/pull-requests/${number}/review`, { method: "POST" }),
    onSuccess: async (response, number) => {
      if (response.queued.includes(number)) toast.success(`Review queued for PR #${number}`)
      else if (response.deduped.includes(number)) toast.info(`PR #${number} already has an active review job`)
      else toast.info(`PR #${number} was not queued`)
      setPr("")
      setRunDialogOpen(false)
      changePage(1)
    },
    onError: (error) => toast.error(error.message),
    onSettled: async () => {
      await queryClient.invalidateQueries({ queryKey: ["projects", project.id], exact: false })
    },
  })
  const refreshLifecycleStatus = useMutation({
    mutationFn: () => api<PullRequestStateRefreshSummary>(`/projects/${project.id}/pull-request-reviews/lifecycle-status/refresh`, { method: "POST" }),
    onError: (error) => toast.error(`Pull request status refresh failed: ${error.message}`),
    onSettled: async () => {
      await queryClient.invalidateQueries({ queryKey: queryKeys.projectPullRequestReviews(project.id), exact: false })
    },
  })
  const startLifecycleRefresh = refreshLifecycleStatus.mutate
  useEffect(() => {
    if (refreshedProject.current === project.id) return
    refreshedProject.current = project.id
    startLifecycleRefresh()
  }, [project.id, startLifecycleRefresh])
  useEffect(() => {
    setSelectedReview(null)
  }, [page])
  useEffect(() => {
    setSelectedReview((selected) => {
      if (!selected || !reviews.data) return selected
      return reviews.data.reviews.find((review) => review.pr === selected.pr) ?? null
    })
  }, [reviews.data])
  useEffect(() => {
    if (!reviews.data) return
    if (reviews.data.total_pages === 0 && page !== 1) changePage(1)
    else if (reviews.data.total_pages > 0 && page > reviews.data.total_pages) changePage(reviews.data.total_pages)
  }, [changePage, page, reviews.data])

  const repository = String(project.repository_full_name || project.repository || "")
  const summary = reviews.data?.summary ?? { active: 0, succeeded: 0, skipped: 0, failed: 0 }

  return <div className="min-h-0 flex-1 overflow-auto">
    <div className="mx-auto flex max-w-6xl flex-col gap-5 p-4 md:p-6">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div><h2 className="text-base font-semibold">Pull request reviews</h2><p className="text-sm text-muted-foreground">Latest state per pull request, with every review job retained in history.</p></div>
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
      {reviews.isLoading && <LoadingState rows={5} />}
      {reviews.error && <ErrorState error={reviews.error} retry={() => void reviews.refetch()} />}
      {reviews.data?.reviews.length === 0 && <EmptyState title="No review jobs yet" description="Queue a pull request review to see its lifecycle and attempts here." action={<Button onClick={() => setRunDialogOpen(true)}><Play data-icon="inline-start" /> Run review</Button>} />}
      {reviews.data && reviews.data.reviews.length > 0 && <>
        <ReviewJobList reviews={reviews.data.reviews} repository={repository} onDetails={setSelectedReview} onRereview={(number) => queue.mutate(number)} pending={queue.isPending} />
        <div className="flex flex-wrap items-center justify-between gap-3"><span className="text-xs text-muted-foreground">{reviews.data.total_items} pull requests</span><PagePagination page={page} totalPages={reviews.data.total_pages} onPageChange={changePage} disabled={reviews.isFetching} label="Pull request review pages" /></div>
      </>}
    </div>
    <ReviewJobDetails projectId={project.id} repository={repository} review={selectedReview} onClose={() => setSelectedReview(null)} onRereview={(number) => queue.mutate(number)} pending={queue.isPending} />
  </div>
}
