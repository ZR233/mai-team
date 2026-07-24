import type { ReviewJobSummary, ReviewRunSummary, TokenUsage } from "@/api/product-types"

const activeStatuses = new Set(["queued", "preparing", "running", "retry_waiting", "submission_pending", "reconciling"])
const usageFields = ["input_tokens", "cached_input_tokens", "output_tokens", "reasoning_output_tokens", "total_tokens"] as const

export interface ReviewUsageProjection {
  total: TokenUsage | null
  attempts: Record<string, TokenUsage>
}

export function summarizeReviewJobs(jobs: ReviewJobSummary[]) {
  return jobs.reduce((summary, job) => {
    if (activeStatuses.has(job.status)) summary.active += 1
    else if (job.status === "succeeded") summary.succeeded += 1
    else if (job.status === "failed") summary.failed += 1
    else if (job.status === "skipped") summary.skipped += 1
    return summary
  }, { active: 0, succeeded: 0, failed: 0, skipped: 0 })
}

export function reviewJobIsActive(job: ReviewJobSummary) {
  return activeStatuses.has(job.status)
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

export function projectReviewUsage(attempts: ReviewRunSummary[]): ReviewUsageProjection {
  const sorted = attempts
    .map((attempt, position) => ({ attempt, position }))
    .sort((left, right) => compareAttempts(left.attempt, right.attempt) || left.position - right.position)
  const attemptsById: Record<string, TokenUsage> = {}
  const total = emptyUsage()
  let hasUsage = false
  let previous: { reviewerAgentId: string; usage: TokenUsage } | null = null

  for (const { attempt } of sorted) {
    const current = normalizeUsage(attempt.token_usage)
    if (!current || usageIsEmpty(current)) continue

    const reviewerAgentId = attempt.reviewer_agent_id || ""
    const previousUsage = reviewerAgentId && previous?.reviewerAgentId === reviewerAgentId ? previous.usage : null
    const canSubtract = previousUsage && usageFields.every((field) => current[field] >= previousUsage[field])
    const incremental = canSubtract ? subtractUsage(current, previousUsage) : current
    previous = reviewerAgentId ? { reviewerAgentId, usage: current } : null
    if (usageIsEmpty(incremental)) continue

    attemptsById[attempt.id] = incremental
    addUsage(total, incremental)
    hasUsage = true
  }

  return { total: hasUsage ? total : null, attempts: attemptsById }
}

export function cacheHitRate(usage?: TokenUsage | null) {
  if (!usage || usage.input_tokens <= 0) return null
  return Math.min(100, Math.max(0, usage.cached_input_tokens / usage.input_tokens * 100))
}

function compareAttempts(left: ReviewRunSummary, right: ReviewRunSummary) {
  const indexDifference = (left.attempt_index ?? 0) - (right.attempt_index ?? 0)
  if (indexDifference !== 0) return indexDifference
  return left.started_at.localeCompare(right.started_at)
}

function normalizeUsage(usage?: TokenUsage): TokenUsage | null {
  if (!usage) return null
  return Object.fromEntries(usageFields.map((field) => [field, normalizeTokenCount(usage[field])])) as unknown as TokenUsage
}

function normalizeTokenCount(value: number) {
  return Number.isFinite(value) ? Math.max(0, Math.trunc(value)) : 0
}

function emptyUsage(): TokenUsage {
  return { input_tokens: 0, cached_input_tokens: 0, output_tokens: 0, reasoning_output_tokens: 0, total_tokens: 0 }
}

function usageIsEmpty(usage: TokenUsage) {
  return usageFields.every((field) => usage[field] === 0)
}

function subtractUsage(current: TokenUsage, previous: TokenUsage): TokenUsage {
  return Object.fromEntries(usageFields.map((field) => [field, current[field] - previous[field]])) as unknown as TokenUsage
}

function addUsage(total: TokenUsage, usage: TokenUsage) {
  for (const field of usageFields) total[field] += usage[field]
}
