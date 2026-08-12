import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { fireEvent, render, screen, waitFor } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { useState } from "react"
import { afterEach, describe, expect, it, vi } from "vitest"

import type { ProjectDetail, PullRequestReviewSummary, ReviewJobSummary } from "@/api/product-types"

import { ReviewJobDetails } from "./review-job-details"
import { ReviewPanel } from "./review-panel"

const toast = vi.hoisted(() => ({ error: vi.fn(), info: vi.fn(), success: vi.fn() }))
vi.mock("sonner", () => ({ toast }))

afterEach(() => {
  vi.unstubAllGlobals()
  vi.clearAllMocks()
})

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

  it("shows merged as the aggregate result and removes re-review actions", async () => {
    vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
      const path = String(input)
      if (path.endsWith("/merge-status/refresh")) return jsonResponse({ checked: 0, newly_merged: 0 })
      if (path.includes("/pull-request-reviews/42/history")) return jsonResponse({
        items: [{ job: approvedJob("approved-job", 42), has_attempts: false }],
        page: 1,
        page_size: 20,
        total_items: 1,
        total_pages: 1,
      })
      return jsonResponse({
        ...reviewPage([review(42, approvedJob("approved-job", 42), 1, "merged")], 1),
        total_items: 1,
        total_pages: 1,
      })
    }))
    renderWithQuery(<ReviewPanelHarness />)

    expect(await screen.findAllByText("Merged")).not.toHaveLength(0)
    await userEvent.click(screen.getAllByRole("button", { name: "Actions for PR #42" })[0])
    expect(screen.queryByRole("menuitem", { name: "Re-review" })).not.toBeInTheDocument()
    await userEvent.click(screen.getByRole("menuitem", { name: "View details" }))
    expect(await screen.findByText("Pull request merged")).toBeVisible()
    expect(screen.getByText("Approved")).toBeVisible()
    expect(screen.queryByRole("button", { name: "Re-review" })).not.toBeInTheDocument()
  })

  it("renders local results before the background merge refresh and refetches once complete", async () => {
    let merged = false
    let listRequests = 0
    let refreshRequests = 0
    let releaseRefresh: () => void = () => undefined
    const refreshGate = new Promise<void>((resolve) => { releaseRefresh = resolve })
    vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
      const path = String(input)
      if (path.endsWith("/merge-status/refresh")) {
        refreshRequests += 1
        await refreshGate
        merged = true
        return jsonResponse({ checked: 1, newly_merged: 1 })
      }
      listRequests += 1
      return jsonResponse({
        ...reviewPage([review(42, approvedJob("approved-job", 42), 1, merged ? "merged" : "not_merged")], 1),
        total_items: 1,
        total_pages: 1,
      })
    }))
    renderWithQuery(<ReviewPanelHarness />)

    expect(await screen.findAllByText("Approved")).not.toHaveLength(0)
    expect(refreshRequests).toBe(1)
    releaseRefresh()
    expect(await screen.findAllByText("Merged")).not.toHaveLength(0)
    expect(listRequests).toBeGreaterThanOrEqual(2)
    expect(refreshRequests).toBe(1)
  })

  it("keeps local review data and refetches after a merge refresh failure", async () => {
    let listRequests = 0
    vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
      const path = String(input)
      if (path.endsWith("/merge-status/refresh")) return jsonResponse({ error: "GitHub unavailable" }, 503)
      listRequests += 1
      return jsonResponse({
        ...reviewPage([review(42, approvedJob("approved-job", 42))], 1),
        total_items: 1,
        total_pages: 1,
      })
    }))
    renderWithQuery(<ReviewPanelHarness />)

    expect(await screen.findAllByText("Approved")).not.toHaveLength(0)
    await waitFor(() => expect(listRequests).toBeGreaterThanOrEqual(2))
    expect(screen.getAllByText("PR #42").length).toBeGreaterThan(0)
  })

  it("surfaces a merged conflict from re-review without retrying the mutation", async () => {
    let queueRequests = 0
    vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = String(input)
      if (path.endsWith("/merge-status/refresh")) return jsonResponse({ checked: 1, newly_merged: 0 })
      if (path.endsWith("/pull-requests/42/review") && init?.method === "POST") {
        queueRequests += 1
        return jsonResponse({ error: "pull request #42 is already merged" }, 409)
      }
      return jsonResponse({
        ...reviewPage([review(42, approvedJob("approved-job", 42))], 1),
        total_items: 1,
        total_pages: 1,
      })
    }))
    renderWithQuery(<ReviewPanelHarness />)

    expect(await screen.findAllByText("Approved")).not.toHaveLength(0)
    await userEvent.click(screen.getAllByRole("button", { name: "Actions for PR #42" })[0])
    await userEvent.click(screen.getByRole("menuitem", { name: "Re-review" }))

    await waitFor(() => expect(toast.error).toHaveBeenCalledWith("pull request #42 is already merged"))
    expect(queueRequests).toBe(1)
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

function approvedJob(id: string, pr: number): ReviewJobSummary {
  const value = job(id, pr, "succeeded", 1)
  value.submission_receipt = {
    github_review_id: pr,
    event: "approve",
    head_sha: value.head_sha,
    submitted_at: value.updated_at,
  }
  return value
}

function review(pr: number, latestJob: ReviewJobSummary, historyCount = 1, mergeState: PullRequestReviewSummary["merge_state"] = "not_merged"): PullRequestReviewSummary {
  return { pr, latest_job: latestJob, history_count: historyCount, merge_state: mergeState, merged_at: mergeState === "merged" ? "2026-08-12T00:00:00Z" : null }
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
