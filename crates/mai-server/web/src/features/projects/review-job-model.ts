import type { ReviewJobSummary, ReviewRunSummary } from "@/api/product-types"

const activeStatuses = new Set(["queued", "preparing", "running", "retry_waiting", "submission_pending", "reconciling"])

export function summarizeReviewJobs(jobs: ReviewJobSummary[]) {
  return jobs.reduce((summary, job) => {
    if (activeStatuses.has(job.status)) summary.active += 1
    else if (job.status === "succeeded") summary.succeeded += 1
    else if (job.status === "failed") summary.failed += 1
    else if (job.status === "skipped") summary.skipped += 1
    return summary
  }, { active: 0, succeeded: 0, failed: 0, skipped: 0 })
}

export function reviewSkipReasonLabel(reason?: ReviewJobSummary["skip_reason"]) {
  switch (reason) {
    case "pull_request_closed": return "Pull request closed"
    case "draft": return "Pull request is a draft"
    case "ci_pending": return "Required checks are pending"
    case "already_reviewed_current_head": return "Current head already reviewed"
    case null:
    case undefined:
      return "No longer eligible"
  }
}

export function latestReviewAttempt(attempts: ReviewRunSummary[]) {
  return attempts.reduce<ReviewRunSummary | null>((latest, attempt) => {
    if (!latest) return attempt
    const currentIndex = attempt.attempt_index ?? 0
    const latestIndex = latest.attempt_index ?? 0
    if (currentIndex !== latestIndex) return currentIndex > latestIndex ? attempt : latest
    return attempt.started_at > latest.started_at ? attempt : latest
  }, null)
}
