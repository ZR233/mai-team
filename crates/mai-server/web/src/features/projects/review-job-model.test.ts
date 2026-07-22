import { describe, expect, it } from "vitest"

import type { ReviewJobSummary, ReviewRunSummary } from "@/api/product-types"

import { latestReviewAttempt, summarizeReviewJobs } from "./review-job-model"

describe("review job presentation model", () => {
  it("keeps retry waiting jobs active instead of counting them as failures", () => {
    const jobs = [
      job("retry_waiting"),
      job("succeeded"),
      job("failed"),
      job("cancelled"),
      job("superseded"),
    ]

    expect(summarizeReviewJobs(jobs)).toEqual({ active: 1, succeeded: 1, failed: 1 })
  })

  it("selects the highest attempt index independently of response order", () => {
    const attempts = [attempt(2), attempt(1), attempt(4), attempt(3)]

    expect(latestReviewAttempt(attempts)?.attempt_index).toBe(4)
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

function attempt(index: number): ReviewRunSummary {
  return {
    id: `attempt-${index}`,
    attempt_index: index,
    status: "succeeded",
    started_at: `2026-07-22T00:0${index}:00Z`,
  }
}
