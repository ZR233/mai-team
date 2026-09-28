import { render, screen, within } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { describe, expect, it } from "vitest"

import type { ReviewRunDetail } from "@/api/product-types"
import type { ThreadItem } from "@/events/thread-events.generated"

import { ReviewActivityList } from "./review-activity-list"
import { buildReviewActivity } from "./review-activity"

describe("Review PL Thread activity", () => {
  it("复用会话 Timeline 展示 Review 历史", () => {
    const detail: ReviewRunDetail = {
      id: "run-1",
      status: "succeeded",
      started_at: "2026-08-26T00:00:00Z",
      finished_at: "2026-08-26T00:01:00Z",
      outcome: "review_submitted",
      review_event: "approve",
      summary: "Review completed",
      usage: { promptTokens: 0, cachedPromptTokens: 0, cacheWriteTokens: 0, completionTokens: 0, reasoningTokens: 0, totalTokens: 0 },
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

  it("keeps a running PL turn distinct from a completed review", () => {
    const detail: ReviewRunDetail = {
      id: "run-1",
      status: "running",
      started_at: "2026-08-26T00:00:00Z",
      usage: { promptTokens: 0, cachedPromptTokens: 0, cacheWriteTokens: 0, completionTokens: 0, reasoningTokens: 0, totalTokens: 0 },
      history_status: "available",
      history: null,
    }

    render(<ReviewActivityList activity={buildReviewActivity(detail)} />)

    expect(screen.getByText(/PL turn is still running/)).toBeVisible()
    expect(screen.getByText(/conclusion will appear/)).toBeVisible()
    expect(screen.queryByText(/without a written summary/)).not.toBeInTheDocument()
  })

  it("pages a long Review chat with the shared pagination control", async () => {
    const items: ThreadItem[] = Array.from({ length: 41 }, (_, index) => ({
      id: `message-${index + 1}`,
      threadId: "thread-1",
      turnId: "turn-1",
      ordinal: index + 1,
      revision: 1,
      createdAt: index + 1,
      updatedAt: index + 1,
      state: { kind: "text", data: { channel: "user", text: `message ${index + 1}`, lifecycle: { kind: "completed", data: { completedAt: index + 1 } } } },
    }))
    render(<ReviewActivityList activity={{ items, status: "succeeded", conclusion: { summary: "done" } }} />)

    expect(screen.getByRole("navigation", { name: "Review activity pages" })).toBeVisible()
    expect(screen.getByText("message 41")).toBeVisible()
    expect(screen.queryByText("message 1")).not.toBeInTheDocument()
    await userEvent.click(screen.getByRole("button", { name: "Previous page" }))
    expect(screen.getByText("message 1")).toBeVisible()
    expect(screen.queryByText("message 41")).not.toBeInTheDocument()
  })
})
