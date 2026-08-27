import { render, screen } from "@testing-library/react"
import { describe, expect, it } from "vitest"

import type { TokenUsage } from "@/api/product-types"

import { ReviewAttemptUsage, ReviewUsageSummary } from "./review-usage-summary"

const usage: TokenUsage = {
  promptTokens: 1_200,
  cachedPromptTokens: 400,
  cacheWriteTokens: 0,
  completionTokens: 180,
  reasoningTokens: 80,
  totalTokens: 1_380,
}

describe("review usage summary", () => {
  it("renders the total, exact breakdown, and accessible cache progress", () => {
    render(<ReviewUsageSummary usage={usage} active={false} />)

    expect(screen.getByText("1.38K")).toHaveAttribute("title", "1,380 tokens")
    expect(screen.getAllByText("33.3%")).toHaveLength(2)
    expect(screen.getByText("1,200")).toBeVisible()
    expect(screen.getByText("400")).toBeVisible()
    expect(screen.getByText("180")).toBeVisible()
    expect(screen.getByText("80")).toBeVisible()
    expect(screen.getByRole("progressbar", { name: "Cache hit rate 33.3%" })).toHaveAttribute("aria-valuenow", "33.33333333333333")
  })

  it("explains why an active review has no persisted usage yet", () => {
    render(<ReviewUsageSummary usage={null} active />)

    expect(screen.getByText("Usage will appear after the active attempt finishes.")).toBeVisible()
    expect(screen.queryByRole("progressbar")).not.toBeInTheDocument()
  })

  it("marks active totals as partial when earlier attempts have usage", () => {
    render(<ReviewUsageSummary usage={usage} active />)

    expect(screen.getByText("Usage from completed attempts; the active attempt appears after it finishes.")).toBeVisible()
  })

  it("renders a compact per-attempt increment", () => {
    render(<ReviewAttemptUsage usage={{ ...usage, promptTokens: 500, cachedPromptTokens: 200, totalTokens: 600 }} />)

    expect(screen.getByText("600 tokens")).toHaveAttribute("title", "600 tokens")
    expect(screen.getByText("40% cache")).toBeVisible()
  })
})
