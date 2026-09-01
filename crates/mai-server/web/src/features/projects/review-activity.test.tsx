import { render, screen, within } from "@testing-library/react"
import { describe, expect, it } from "vitest"

import type { ReviewRunDetail } from "@/api/product-types"

import { ReviewActivityList } from "./review-activity-list"
import { buildReviewActivity } from "./review-activity"

describe("Review PL v2 归档展示", () => {
  it("复用会话 Timeline 展示 Review 历史", () => {
    const detail: ReviewRunDetail = {
      id: "run-1",
      status: "succeeded",
      started_at: "2026-08-26T00:00:00Z",
      finished_at: "2026-08-26T00:01:00Z",
      outcome: "review_submitted",
      review_event: "approve",
      summary: "Review completed",
      history_status: "available",
      history: {
        turn: {
          id: "turn-1",
          threadId: "thread-1",
          revision: 1,
          state: { kind: "completed", data: { startedAt: 1, completedAt: 2, completion: "normal" } },
          updatedAt: 2,
        },
        contextDisposition: "active",
        items: [
          {
            id: "user-1",
            threadId: "thread-1",
            turnId: "turn-1",
            ordinal: 1,
            revision: 1,
            createdAt: 1,
            updatedAt: 1,
            state: { kind: "text", data: { channel: "user", text: "Review this PR", lifecycle: { kind: "completed", data: { completedAt: 1 } } } },
          },
          {
            id: "final-1",
            threadId: "thread-1",
            turnId: "turn-1",
            ordinal: 2,
            revision: 1,
            createdAt: 2,
            updatedAt: 2,
            state: { kind: "text", data: { channel: "final", text: "Shared review response", lifecycle: { kind: "completed", data: { completedAt: 2 } } } },
          },
        ],
      },
    }

    render(<ReviewActivityList activity={buildReviewActivity(detail)} />)

    const timeline = screen.getByRole("feed", { name: "Conversation timeline" })
    expect(within(timeline).getByRole("article", { name: "You message" })).toHaveTextContent("Review this PR")
    expect(within(timeline).getByRole("article", { name: "Mai Team response" })).toHaveTextContent("Shared review response")
  })

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
