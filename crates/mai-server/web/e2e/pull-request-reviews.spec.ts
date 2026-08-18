import { expect, test, type Page, type Route } from "@playwright/test"

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    class QuietEventSource extends EventTarget {
      static readonly CONNECTING = 0
      static readonly OPEN = 1
      static readonly CLOSED = 2
      readonly url: string
      readonly withCredentials = false
      readyState = QuietEventSource.OPEN
      onopen: ((event: Event) => void) | null = null
      onmessage: ((event: MessageEvent) => void) | null = null
      onerror: ((event: Event) => void) | null = null
      constructor(url: string | URL) {
        super()
        this.url = String(url)
        window.setTimeout(() => this.onopen?.(new Event("open")), 0)
      }
      close() { this.readyState = QuietEventSource.CLOSED }
    }
    Object.defineProperty(window, "EventSource", { configurable: true, value: QuietEventSource })
  })
  await installReviewFixture(page)
})

test("PR 聚合列表分页并在详情切换执行和未执行历史", async ({ page }) => {
  await page.goto("/projects/project-1?view=review")

  await expect(pullRequestEntry(page, 42)).toBeVisible()
  await expect(page.getByLabel("Review job summary")).toContainText("1 succeeded")
  await pullRequestEntry(page, 42).click()
  await expect(page.getByText("此记录未启动 Agent 会话")).toBeVisible()

  await page.getByRole("combobox", { name: "Review history" }).click()
  await page.getByRole("option", { name: /succeeded/i }).click()
  await expect(page.getByText("Attempt 1")).toBeVisible()
  await expect(page.getByText("Review completed").first()).toBeVisible()

  await page.keyboard.press("Escape")
  await expect(page.getByRole("dialog")).toHaveCount(0)
  const pagination = page.getByRole("navigation", { name: "Pull request review pages" })
  const next = (page.viewportSize()?.width ?? 0) < 640
    ? pagination.getByRole("button", { name: /^next$/i })
    : pagination.getByRole("button", { name: "Next page" })
  await next.click()
  await expect(page).toHaveURL(/review_page=2/)
  await expect(pullRequestEntry(page, 41)).toBeVisible()
  await expect(page.getByText("PR #42")).toHaveCount(0)
})

test("Merged 覆盖主列表结果并保留原始 review 历史", async ({ page }) => {
  await page.goto("/projects/project-1?view=review&review_page=2")

  await expect(pullRequestEntry(page, 41)).toBeVisible()
  const merged = (page.viewportSize()?.width ?? 0) < 1024
    ? pullRequestEntry(page, 41).getByText("Merged")
    : page.getByRole("table").getByText("Merged")
  await expect(merged).toBeVisible()
  await pullRequestEntry(page, 41).click()
  await expect(page.getByText("Pull request merged")).toBeVisible()
  await expect(page.getByText("Approved", { exact: true })).toBeVisible()
  await expect(page.getByRole("button", { name: "Re-review" })).toHaveCount(0)

  await page.keyboard.press("Escape")
  await pullRequestEntry(page, 40).click()
  await expect(page.getByText("Pull request closed")).toBeVisible()
  await expect(page.getByText("Approved", { exact: true })).toBeVisible()
  await expect(page.getByRole("button", { name: "Re-review" })).toHaveCount(0)
})

function pullRequestEntry(page: Page, pr: number) {
  return (page.viewportSize()?.width ?? 0) < 1024
    ? page.locator("button.min-w-0").filter({ hasText: `PR #${pr}` }).first()
    : page.getByText(`PR #${pr}`).first()
}

async function installReviewFixture(page: Page) {
  await page.route("**/*", async (route) => {
    const request = route.request()
    if (request.isNavigationRequest()) return route.continue()
    const url = new URL(request.url())
    const path = url.pathname
    if (path === "/projects") return json(route, [projectSummary])
    if (path === "/projects/project-1") return json(route, projectDetail())
    if (path === "/projects/project-1/pull-request-reviews") {
      const pageNumber = Number(url.searchParams.get("page") || "1")
      return json(route, reviewPage(pageNumber))
    }
    if (path === "/projects/project-1/pull-request-reviews/lifecycle-status/refresh") {
      return json(route, { checked: 2, newly_merged: 0, newly_closed: 0 })
    }
    if (path === "/projects/project-1/pull-request-reviews/41/history") return json(route, {
      items: [{ job: approvedReviewJob("approved-job", 41) }],
      page: 1,
      page_size: 20,
      total_items: 1,
      total_pages: 1,
    })
    if (path === "/projects/project-1/pull-request-reviews/40/history") return json(route, {
      items: [{ job: approvedReviewJob("closed-job", 40) }],
      page: 1,
      page_size: 20,
      total_items: 1,
      total_pages: 1,
    })
    if (path === "/projects/project-1/pull-request-reviews/42/history") return json(route, {
      items: [
        { job: reviewJob("skipped-job", 42, "skipped", 0) },
        { job: reviewJob("executed-job", 42, "succeeded", 1) },
      ],
      page: 1,
      page_size: 20,
      total_items: 2,
      total_pages: 1,
    })
    if (path === "/projects/project-1/review-jobs/executed-job") return json(route, {
      ...reviewJob("executed-job", 42, "succeeded", 1),
      attempts: [reviewRun],
    })
    if (path === "/projects/project-1/review-runs/run-1") return json(route, { ...reviewRun, history: null })
    return route.continue()
  })
}

function reviewPage(page: number) {
  const reviews = page === 2
    ? [
      { pr: 41, latest_job: approvedReviewJob("approved-job", 41), history_count: 1, lifecycle_state: "merged", state_changed_at: "2026-08-12T00:00:00Z" },
      { pr: 40, latest_job: approvedReviewJob("closed-job", 40), history_count: 1, lifecycle_state: "closed", state_changed_at: "2026-08-12T00:30:00Z" },
    ]
    : [{ pr: 42, latest_job: reviewJob("skipped-job", 42, "skipped", 0), history_count: 2, lifecycle_state: "open", state_changed_at: null }]
  return {
    reviews,
    page,
    page_size: 20,
    total_items: 21,
    total_pages: 2,
    summary: { active: 0, succeeded: 1, skipped: 19, failed: 1 },
  }
}

function reviewJob(id: string, pr: number, status: string, attemptCount: number) {
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

function approvedReviewJob(id: string, pr: number) {
  const value = reviewJob(id, pr, "succeeded", 1)
  return {
    ...value,
    submission_receipt: {
      github_review_id: pr,
      event: "approve",
      head_sha: value.head_sha,
      submitted_at: value.updated_at,
    },
  }
}

const reviewRun = {
  id: "run-1",
  job_id: "executed-job",
  attempt_index: 1,
  status: "completed",
  started_at: "2026-08-11T10:00:00Z",
  finished_at: "2026-08-11T10:01:00Z",
  summary: "Review completed",
}

const projectSummary = {
  id: "project-1",
  name: "owner/repo",
  repository: "owner/repo",
  repository_full_name: "owner/repo",
  status: "ready",
  maintainer_agent_id: "agent-1",
}

function projectDetail() {
  const agent = {
    id: "agent-1",
    name: "Maintainer",
    role: "planner",
    state: { resource: "ready", runtime: { lifecycle: "active", activity: "idle", active_turn: null, pending_inputs: 0 } },
    provider_id: "fixture",
    provider_name: "Fixture",
    model: "fixture-model",
    created_at: "2026-08-11T00:00:00Z",
    updated_at: "2026-08-11T00:00:00Z",
    token_usage: { input_tokens: 0, cached_input_tokens: 0, output_tokens: 0, reasoning_output_tokens: 0, total_tokens: 0 },
    thread: { id: "agent-1", projectId: "project-1", title: "Maintainer", mode: "simple", rootThreadId: "agent-1", role: "planner", agentPath: "root", status: "idle", createdAt: 1, updatedAt: 1, archived: false },
  }
  return { ...projectSummary, maintainer_agent: agent, agents: [agent], selected_agent_id: agent.id, selected_agent: agent, review_runs: [] }
}

function json(route: Route, body: unknown) {
  return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(body) })
}
