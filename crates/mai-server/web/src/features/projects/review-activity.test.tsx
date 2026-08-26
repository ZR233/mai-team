import { render, screen } from "@testing-library/react"
import { describe, expect, it } from "vitest"

import type { ReviewRunDetail } from "@/api/product-types"

import { ReviewActivityList } from "./review-activity-list"
import { buildReviewActivity } from "./review-activity"

describe("Review PL v2 归档展示", () => {
  it("把离线 Timeline 显示为归档状态而不是加载失败", () => {
    const detail: ReviewRunDetail = {
      id: "run-1",
      status: "succeeded",
      started_at: "2026-08-26T00:00:00Z",
      finished_at: "2026-08-26T00:01:00Z",
      outcome: "review_submitted",
      review_event: "approve",
      summary: "Review completed",
      history_status: "pl_v2_archived",
      history_archive_id: "pl-v2-20260826",
      history_archived_at: "2026-08-26T01:00:00Z",
      history: null,
    }

    render(<ReviewActivityList activity={buildReviewActivity(detail)} />)

    expect(screen.getByText("PL v2 升级离线归档")).toBeVisible()
    expect(screen.getByText("pl-v2-20260826")).toBeVisible()
    expect(screen.getByText("Approved")).toBeVisible()
    expect(screen.queryByText(/failed to load/i)).not.toBeInTheDocument()
  })
})
