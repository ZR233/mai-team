import { describe, expect, it } from "vitest"

import type { ReviewJobSummary, ReviewRunSummary, TokenUsage } from "@/api/product-types"

import { cacheHitRate, latestReviewAttempt, projectReviewUsage, reviewJobIsActive, reviewSkipReasonLabel, summarizeReviewJobs } from "./review-job-model"

describe("review job presentation model", () => {
  it("keeps retry waiting jobs active instead of counting them as failures", () => {
    const jobs = [
      job("retry_waiting"),
      job("succeeded"),
      job("failed"),
      job("cancelled"),
      job("superseded"),
      job("skipped"),
    ]

    expect(summarizeReviewJobs(jobs)).toEqual({ active: 1, succeeded: 1, failed: 1, skipped: 1 })
    expect(reviewJobIsActive(jobs[0])).toBe(true)
    expect(reviewJobIsActive(jobs[1])).toBe(false)
  })

  it("explains why a job was skipped before creating an attempt", () => {
    expect(reviewSkipReasonLabel("already_reviewed_current_head")).toBe("Current head already reviewed")
    expect(reviewSkipReasonLabel("ci_pending")).toBe("Required checks are pending")
  })

  it("selects the highest attempt index independently of response order", () => {
    const attempts = [attempt(2), attempt(1), attempt(4), attempt(3)]

    expect(latestReviewAttempt(attempts)?.attempt_index).toBe(4)
  })

  it("projects cumulative reviewer usage into stable attempt increments", () => {
    const attempts = [
      attempt(2, usage(1_200, 400, 180, 80, 1_380)),
      attempt(1, usage(700, 200, 80, 30, 780)),
    ]

    expect(projectReviewUsage(attempts)).toEqual({
      total: usage(1_200, 400, 180, 80, 1_380),
      attempts: {
        "attempt-1": usage(700, 200, 80, 30, 780),
        "attempt-2": usage(500, 200, 100, 50, 600),
      },
    })
  })

  it("starts a new usage baseline when the reviewer changes or counters reset", () => {
    const attempts = [
      attempt(1, usage(100, 20, 10, 5, 110), "reviewer-a"),
      attempt(2, usage(60, 10, 5, 2, 65), "reviewer-b"),
      attempt(3, usage(20, 4, 2, 1, 22), "reviewer-b"),
    ]

    expect(projectReviewUsage(attempts)).toEqual({
      total: usage(180, 34, 17, 8, 197),
      attempts: {
        "attempt-1": usage(100, 20, 10, 5, 110),
        "attempt-2": usage(60, 10, 5, 2, 65),
        "attempt-3": usage(20, 4, 2, 1, 22),
      },
    })
  })

  it("omits missing and empty usage without presenting a zero-cost attempt", () => {
    const attempts = [
      attempt(1),
      attempt(2, usage(0, 0, 0, 0, 0)),
    ]

    expect(projectReviewUsage(attempts)).toEqual({ total: null, attempts: {} })
  })

  it("uses a weighted cache hit rate and guards invalid denominators", () => {
    expect(cacheHitRate(usage(1_200, 400, 180, 80, 1_380))).toBeCloseTo(33.333)
    expect(cacheHitRate(usage(100, 140, 10, 0, 110))).toBe(100)
    expect(cacheHitRate(usage(0, 0, 10, 0, 10))).toBeNull()
    expect(cacheHitRate(null)).toBeNull()
  })
})

function job(status: ReviewJobSummary["status"]): ReviewJobSummary {
  return {
    id: `job-${status}`,
    project_id: "project-1",
    pr: 42,
    head_sha: "head",
    source: "manual",
    reason: "test",
    status,
    attempt_count: 1,
    max_attempts: 5,
    created_at: "2026-07-22T00:00:00Z",
    updated_at: "2026-07-22T00:00:00Z",
  }
}

function attempt(index: number, tokenUsage?: TokenUsage, reviewerAgentId = "reviewer-1"): ReviewRunSummary {
  return {
    id: `attempt-${index}`,
    attempt_index: index,
    status: "succeeded",
    started_at: `2026-07-22T00:0${index}:00Z`,
    reviewer_agent_id: reviewerAgentId,
    token_usage: tokenUsage,
  }
}

function usage(input: number, cached: number, output: number, reasoning: number, total: number): TokenUsage {
  return {
    input_tokens: input,
    cached_input_tokens: cached,
    output_tokens: output,
    reasoning_output_tokens: reasoning,
    total_tokens: total,
  }
}
