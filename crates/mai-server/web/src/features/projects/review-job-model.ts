import type { ReviewJobSummary, ReviewRunSummary } from "@/api/product-types"

const activeStatuses = new Set(["queued", "preparing", "running", "retry_waiting", "submission_pending", "reconciling"])

export function summarizeReviewJobs(jobs: ReviewJobSummary[]) {
  return jobs.reduce((summary, job) => {
    if (activeStatuses.has(job.status)) summary.active += 1
    else if (job.status === "succeeded") summary.succeeded += 1
    else if (job.status === "failed") summary.failed += 1
    return summary
  }, { active: 0, succeeded: 0, failed: 0 })
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
