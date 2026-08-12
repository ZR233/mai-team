import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { fireEvent, render, screen, waitFor } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { useState } from "react"
import { afterEach, describe, expect, it, vi } from "vitest"

import type { ProjectDetail, PullRequestReviewSummary, ReviewJobSummary } from "@/api/product-types"

import { ReviewJobDetails } from "./review-job-details"
import { ReviewPanel } from "./review-panel"

afterEach(() => vi.unstubAllGlobals())

describe("pull request review pagination", () => {
  it("requests the selected page and replaces the aggregate rows", async () => {
    const requested: string[] = []
    vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
      const path = String(input)
      requested.push(path)
      const url = new URL(path, "http://test")
      if (url.pathname.endsWith("/pull-request-reviews/42/history")) return jsonResponse({
        items: [{ job: job("job-42", 42, "succeeded", 1), has_attempts: false }],
        page: 1,
        page_size: 20,
        total_items: 1,
        total_pages: 1,
      })
      const page = url.searchParams.get("page")
      return jsonResponse(reviewPage(page === "2" ? [review(41, job("job-41", 41, "failed"))] : [review(42, job("job-42", 42, "succeeded"))], Number(page)))
    }))
    renderWithQuery(<ReviewPanelHarness />)

    expect(await screen.findAllByText("PR #42")).not.toHaveLength(0)
    await userEvent.click(screen.getByRole("button", { name: "Next page" }))

    expect(await screen.findAllByText("PR #41")).not.toHaveLength(0)
    await waitFor(() => expect(screen.queryByText("PR #42")).not.toBeInTheDocument())
    await userEvent.click(screen.getByRole("button", { name: "Previous page" }))
    expect(await screen.findAllByText("PR #42")).not.toHaveLength(0)
    const reviewButton = screen.getAllByText("PR #42").map((element) => element.closest("button")).find(Boolean)
    expect(reviewButton).toBeTruthy()
    await userEvent.click(reviewButton!)
    expect(await screen.findByRole("dialog", { name: /Pull request review/ })).toBeVisible()
    fireEvent.click(screen.getByTestId("force-review-page"))
    await waitFor(() => expect(screen.queryByRole("dialog", { name: /Pull request review/ })).not.toBeInTheDocument())
    expect(await screen.findAllByText("PR #41")).not.toHaveLength(0)
    expect(requested).toContain("/projects/project-1/pull-request-reviews?page=2&page_size=20")
  })
})

describe("pull request review history", () => {
  it("defaults to the latest job, avoids details without attempts, and restores executed history", async () => {
    const requested: string[] = []
    vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
      const path = String(input)
      requested.push(path)
      if (path.includes("/pull-request-reviews/42/history")) return jsonResponse({
        items: [
          { job: job("skipped-job", 42, "skipped", 0), has_attempts: false },
          { job: job("executed-job", 42, "succeeded", 1), has_attempts: true },
        ],
        page: 1,
        page_size: 20,
        total_items: 2,
        total_pages: 1,
      })
      if (path.endsWith("/review-jobs/executed-job")) return jsonResponse({
        ...job("executed-job", 42, "succeeded", 1),
        attempts: [{ id: "run-1", job_id: "executed-job", attempt_index: 1, status: "completed", started_at: "2026-08-11T10:00:00Z", finished_at: "2026-08-11T10:01:00Z", summary: "Review completed" }],
      })
      if (path.endsWith("/review-runs/run-1")) return jsonResponse({ id: "run-1", job_id: "executed-job", attempt_index: 1, status: "completed", started_at: "2026-08-11T10:00:00Z", finished_at: "2026-08-11T10:01:00Z", summary: "Review completed", history: null })
      return jsonResponse({ error: `unexpected request ${path}` }, 404)
    }))
    renderWithQuery(<ReviewJobDetails projectId="project-1" repository="owner/repo" review={review(42, job("skipped-job", 42, "skipped"), 2)} onClose={() => undefined} onRereview={() => undefined} pending={false} />)

    expect(await screen.findByText("此记录未启动 Agent 会话")).toBeVisible()
    expect(requested.some((path) => path.endsWith("/review-jobs/skipped-job"))).toBe(false)

    await userEvent.click(screen.getByRole("combobox", { name: "Review history" }))
    await userEvent.click(await screen.findByRole("option", { name: /succeeded/i }))

    expect(await screen.findByText("Attempt 1")).toBeVisible()
    expect(requested.some((path) => path.endsWith("/review-jobs/executed-job"))).toBe(true)
    expect(requested.some((path) => path.endsWith("/review-runs/run-1"))).toBe(true)

    await userEvent.click(screen.getByRole("combobox", { name: "Review history" }))
    await userEvent.click(await screen.findByRole("option", { name: /skipped/i }))
    expect(await screen.findByText("此记录未启动 Agent 会话")).toBeVisible()
  })

  it("selects the first item on each history page and retries a failed history load", async () => {
    let firstPageRequests = 0
    vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
      const path = String(input)
      if (!path.includes("/pull-request-reviews/42/history")) return jsonResponse({ error: "unexpected" }, 404)
      const page = new URL(path, "http://test").searchParams.get("page")
      if (page === "1" && firstPageRequests++ === 0) return jsonResponse({ error: "temporary history failure" }, 500)
      const selected = page === "2" ? job("cancelled-job", 42, "cancelled") : job("latest-job", 42, "skipped")
      return jsonResponse({ items: [{ job: selected, has_attempts: false }], page: Number(page), page_size: 20, total_items: 21, total_pages: 2 })
    }))
    renderWithQuery(<ReviewJobDetails projectId="project-1" repository="owner/repo" review={review(42, job("latest-job", 42, "skipped"), 21)} onClose={() => undefined} onRereview={() => undefined} pending={false} />)

    expect(await screen.findByText("temporary history failure")).toBeVisible()
    await userEvent.click(screen.getByRole("button", { name: "Retry" }))
    expect(await screen.findByText("此记录未启动 Agent 会话")).toBeVisible()
    expect(screen.getByRole("combobox", { name: "Review history" })).toHaveTextContent("latest-j")

    await userEvent.click(screen.getByRole("button", { name: "Next page" }))
    await waitFor(() => expect(screen.getByRole("combobox", { name: "Review history" })).toHaveTextContent("cancelled"))
    expect(screen.getByText("此记录未启动 Agent 会话")).toBeVisible()
  })
})

function ReviewPanelHarness() {
  const [page, setPage] = useState(1)
  return <><button type="button" data-testid="force-review-page" onClick={() => setPage(2)}>Force page</button><ReviewPanel project={project} page={page} onPageChange={setPage} /></>
}

const project = {
  id: "project-1",
  name: "owner/repo",
  repository: "owner/repo",
  repository_full_name: "owner/repo",
  status: "ready",
  maintainer_agent_id: "agent-1",
} as ProjectDetail

function job(id: string, pr: number, status: ReviewJobSummary["status"], attemptCount = 0): ReviewJobSummary {
  return {
    id,
    project_id: "project-1",
    pr,
    head_sha: `${id}-head-sha`,
    source: "manual",
    reason: status === "skipped" ? "not eligible" : "test",
    status,
    attempt_count: attemptCount,
    max_attempts: 5,
    created_at: `2026-08-11T1${attemptCount}:00:00Z`,
    updated_at: `2026-08-11T1${attemptCount}:00:00Z`,
  }
}

function review(pr: number, latestJob: ReviewJobSummary, historyCount = 1): PullRequestReviewSummary {
  return { pr, latest_job: latestJob, history_count: historyCount }
}

function reviewPage(reviews: PullRequestReviewSummary[], page: number) {
  return {
    reviews,
    page,
    page_size: 20,
    total_items: 21,
    total_pages: 2,
    summary: { active: 0, succeeded: 1, skipped: 0, failed: 1 },
  }
}

function renderWithQuery(children: React.ReactNode) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } })
  return render(<QueryClientProvider client={client}>{children}</QueryClientProvider>)
}

function jsonResponse(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } })
}
