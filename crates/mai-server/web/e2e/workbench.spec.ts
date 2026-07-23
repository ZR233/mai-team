import { expect, test, type Page, type Route } from "@playwright/test"

test.beforeEach(async ({ page }) => {
  await page.addInitScript(({ frame }) => {
    class FixtureEventSource extends EventTarget {
      static readonly CONNECTING = 0
      static readonly OPEN = 1
      static readonly CLOSED = 2
      readonly url: string
      readonly withCredentials = false
      readyState = FixtureEventSource.OPEN
      onopen: ((event: Event) => void) | null = null
      onmessage: ((event: MessageEvent) => void) | null = null
      onerror: ((event: Event) => void) | null = null

      constructor(url: string | URL) {
        super()
        this.url = String(url)
        window.setTimeout(() => {
          if (this.readyState === FixtureEventSource.CLOSED) return
          this.onopen?.(new Event("open"))
          if (this.url.includes("/sessions/")) {
            this.dispatchEvent(new MessageEvent("snapshot", { data: JSON.stringify(frame) }))
          }
        }, 0)
      }

      close() {
        this.readyState = FixtureEventSource.CLOSED
      }
    }
    Object.defineProperty(window, "EventSource", { configurable: true, value: FixtureEventSource })
  }, { frame: sessionFrame })
  await installApiFixture(page)
})

test("chat snapshot, skill mention, and route navigation stay functional", async ({ page }, testInfo) => {
  const consoleErrors: string[] = []
  page.on("console", (message) => { if (message.type() === "error") consoleErrors.push(message.text()) })
  page.on("pageerror", (error) => consoleErrors.push(error.message))

  await page.goto("/chat/env-1")
  await expect(page).toHaveTitle("Mai Team")
  await expect(page.getByRole("heading", { name: "Review", exact: true })).toBeVisible()
  await expect(page.getByText("Inspect the canonical session stream")).toBeVisible()
  if (testInfo.project.name === "desktop") {
    await expect(page.getByText("Read project constraints", { exact: true })).toBeVisible()
  } else {
    await expect(page.getByRole("button", { name: "Open working list" })).toBeVisible()
  }

  await page.getByRole("button", { name: "Select skills" }).click()
  await page.getByRole("menuitemcheckbox", { name: /review-helper/i }).click()
  await page.keyboard.press("Escape")
  await expect(page.getByRole("button", { name: "Remove review-helper" })).toBeVisible()
  await page.getByPlaceholder(/Send a command/).fill("Continue the review")
  const request = page.waitForRequest((candidate) => candidate.url().includes("/messages") && candidate.method() === "POST")
  await page.getByRole("button", { name: /Send/ }).click()
  expect((await request).postDataJSON()).toMatchObject({ message: "Continue the review", skill_mentions: ["review-helper"] })

  if (testInfo.project.name === "mobile") await page.getByRole("button", { name: "Toggle Sidebar" }).click()
  await page.getByRole("link", { name: "Providers" }).click()
  await expect(page).toHaveURL(/\/providers$/)
  expect(consoleErrors).toEqual([])
  await expect(page.getByRole("heading", { name: "Providers" })).toBeVisible()
  await page.getByRole("button", { name: /Add provider/ }).click()
  await expect(page.getByRole("dialog", { name: "Add provider" })).toBeVisible()
  await expect(page.getByRole("tab")).toHaveCount(2)
  await expect(page).toHaveScreenshot(`provider-editor-${testInfo.project.name}.png`, { animations: "disabled" })
  await page.keyboard.press("Escape")

  if (testInfo.project.name === "mobile") await page.getByRole("button", { name: "Toggle Sidebar" }).click()
  await page.getByRole("link", { name: "Settings" }).click()
  await expect(page).toHaveURL(/\/settings/)
  await expect(page.getByRole("heading", { name: "Role models" })).toBeVisible()
  expect(consoleErrors).toEqual([])
  await expect(page).toHaveScreenshot(`settings-${testInfo.project.name}.png`, { animations: "disabled" })
})

test("chat workbench is usable at each configured viewport", async ({ page }, testInfo) => {
  await page.goto("/chat/env-1")
  await expect(page.getByRole("heading", { name: "Review", exact: true })).toBeVisible()
  await expect(page.getByPlaceholder(/Send a command/)).toBeVisible()
  await expect(page.getByText("Outdated task", { exact: true })).toHaveCount(0)
  await expect(page.getByRole("button", { name: "Expand 4 tool calls" })).toBeVisible()
  await expect(page.getByText("18s", { exact: true })).toBeVisible()
  const timelineViewport = page.locator("[data-session-timeline-scroll] > [data-radix-scroll-area-viewport]")
  await expect.poll(() => timelineViewport.evaluate((element) => element.scrollWidth <= element.clientWidth + 1)).toBe(true)
  await expect(page.getByText("Run command", { exact: true })).toHaveCount(0)
  await expect(page).toHaveScreenshot(`chat-tools-collapsed-${testInfo.project.name}.png`, { animations: "disabled" })
  await page.getByRole("button", { name: "Expand 4 tool calls" }).click()
  await expect(page.getByRole("button", { name: "Collapse 4 tool calls" })).toBeVisible()
  await expect(page.getByText("Run command", { exact: true })).toBeVisible()
  await expect.poll(() => timelineViewport.evaluate((element) => element.scrollWidth <= element.clientWidth + 1)).toBe(true)
  await expect(page).toHaveScreenshot(`chat-${testInfo.project.name}.png`, { animations: "disabled" })

  await page.getByRole("button", { name: "Expand Run command" }).click()
  await expect(page.getByText("Standard output")).toBeVisible()
  await expect(page.getByText("Finished focused review checks")).toBeVisible()
  await expect(page.getByText(/"command":/)).toHaveCount(0)
  await expect(page).toHaveScreenshot(`chat-tool-${testInfo.project.name}.png`, { animations: "disabled" })
  await page.getByRole("button", { name: "Collapse Run command" }).click()

  if (testInfo.project.name === "desktop") {
    const rail = page.locator("[data-session-todo-rail]")
    await expect(rail).toBeVisible()
    await expect(rail).toHaveCSS("position", "sticky")
    await page.getByRole("button", { name: "Collapse working list" }).click()
    await expect(page.getByRole("button", { name: "Expand working list" })).toBeVisible()
    await expect(page).toHaveScreenshot("chat-todo-collapsed-desktop.png", { animations: "disabled" })
  } else {
    await page.getByRole("button", { name: "Open working list" }).click()
    const drawer = page.locator('[data-slot="drawer-content"]')
    await expect(drawer.getByRole("heading", { name: "Working list" })).toBeVisible()
    await expect(drawer.getByText("Run focused checks", { exact: true })).toBeVisible()
    await expect(page).toHaveScreenshot(`chat-todo-drawer-${testInfo.project.name}.png`, { animations: "disabled" })
    await drawer.getByRole("button", { name: "Close working list" }).click()
    await expect(drawer).not.toBeVisible()
  }
})

test("review actions remain available at each configured viewport", async ({ page }, testInfo) => {
  await page.goto("/projects/project-1")
  await page.getByRole("tab", { name: "Review", exact: true }).first().click()
  await expect(page.getByRole("heading", { name: "Pull request reviews" })).toBeVisible()
  await expect(page.getByRole("link", { name: "Open PR #1631" })).toBeVisible()
  const skipped = page.getByText("Current head already reviewed", { exact: true })
  await expect(testInfo.project.name === "desktop" ? skipped.first() : skipped.last()).toBeVisible()
  const approved = page.getByText("Approved", { exact: true })
  const requestChanges = page.getByText("Request changes", { exact: true })
  await expect(testInfo.project.name === "desktop" ? approved.first() : approved.last()).toBeVisible()
  await expect(testInfo.project.name === "desktop" ? requestChanges.first() : requestChanges.last()).toBeVisible()
  await expect(page).toHaveScreenshot(`review-list-${testInfo.project.name}.png`, { animations: "disabled" })

  await page.getByRole("button", { name: "Actions for PR #1631" }).click()
  await page.getByRole("menuitem", { name: "View details" }).click()
  await expect(page.getByRole("heading", { name: "Review job · PR #1631", exact: true })).toBeVisible()
  await expect(page.getByRole("link", { name: "Open pull request" })).toBeVisible()
  await expect(page.getByRole("button", { name: "Re-review" })).toBeVisible()
  await expect(page.getByRole("heading", { name: "Attempt activity" })).toBeVisible()
  await expect(page.getByText("Run command", { exact: true })).toBeVisible()
  await expect(page.getByText("Read session note", { exact: true })).toBeVisible()
  await expect(page.getByText("GitHub API request", { exact: true })).toBeVisible()
  await expect(page.getByText(/"stdout":/)).toHaveCount(0)
  await expect(page).toHaveScreenshot(`review-${testInfo.project.name}.png`, { animations: "disabled" })
  await page.getByRole("button", { name: "Expand Run command" }).click()
  await expect(page.getByText("All reviewer checks passed")).toBeVisible()
  await expect(page.getByText(/"exitCode":/)).toHaveCount(0)
  await expect(page).toHaveScreenshot(`review-tool-${testInfo.project.name}.png`, { animations: "disabled" })
  await page.keyboard.press("Escape")

  const review = page.waitForRequest((candidate) => candidate.url().endsWith("/projects/project-1/pull-requests/1631/review") && candidate.method() === "POST")
  await page.getByRole("button", { name: "Actions for PR #1631" }).click()
  await page.getByRole("menuitem", { name: "Re-review" }).click()
  await review
})

test("project review, repository settings, and default-branch skills remain functional", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "desktop", "CRUD contract coverage runs once on desktop")
  await page.goto("/projects/project-1")
  await expect(page.getByLabel("breadcrumb").getByText("TGOS Kits", { exact: true })).toBeVisible()
  await expect(page.getByRole("heading", { name: "Agents", exact: true })).toBeVisible()

  await page.getByRole("tab", { name: "Review", exact: true }).first().click()
  await expect(page.getByRole("heading", { name: "Pull request reviews" })).toBeVisible()
  await expect(page.getByRole("cell", { name: "PR #1631", exact: true })).toBeVisible()
  await expect(page.getByRole("link", { name: "Open PR #1631" })).toBeVisible()
  await page.getByRole("button", { name: "Run review" }).click()
  await page.getByPlaceholder("1631").fill("1631")
  const review = page.waitForRequest((candidate) => candidate.url().endsWith("/projects/project-1/pull-requests/1631/review") && candidate.method() === "POST")
  await page.getByRole("button", { name: "Queue review" }).click()
  await review
  await page.getByRole("button", { name: "Actions for PR #1631" }).click()
  await page.getByRole("menuitem", { name: "View details" }).click()
  await expect(page.getByRole("heading", { name: "Review job · PR #1631", exact: true })).toBeVisible()
  await expect(page.getByRole("heading", { name: "Attempt activity", exact: true })).toBeVisible()
  await expect(page.getByRole("button", { name: "Re-review" })).toBeVisible()
  await page.keyboard.press("Escape")

  await page.getByRole("tab", { name: "Repository" }).click()
  await page.getByLabel("Name").fill("TGOS Kits Updated")
  const save = page.waitForRequest((candidate) => candidate.url().endsWith("/projects/project-1") && candidate.method() === "PATCH")
  await page.getByRole("button", { name: "Save changes" }).click()
  expect((await save).postDataJSON()).toMatchObject({ name: "TGOS Kits Updated" })

  await page.getByRole("tab", { name: "Skills" }).click()
  await expect(page.getByText("review-helper", { exact: true })).toBeVisible()
  const detect = page.waitForRequest((candidate) => candidate.url().endsWith("/projects/project-1/skills/detect") && candidate.method() === "POST")
  await page.getByRole("button", { name: "Detect" }).click()
  await detect
})

test("task approval and revision actions keep their API contracts", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "desktop", "CRUD contract coverage runs once on desktop")
  await page.goto("/tasks/task-1")
  await expect(page.getByRole("heading", { name: "Unify session events" })).toBeVisible()
  await expect(page.getByRole("heading", { name: "Implement canonical reducer", exact: true })).toBeVisible()

  const approve = page.waitForRequest((candidate) => candidate.url().endsWith("/tasks/task-1/plan:approve") && candidate.method() === "POST")
  await page.getByRole("button", { name: "Approve plan" }).click()
  await approve

  await page.getByPlaceholder("Revision feedback").fill("Add reconnect coverage")
  const revision = page.waitForRequest((candidate) => candidate.url().endsWith("/tasks/task-1/plan:request-revision") && candidate.method() === "POST")
  await page.getByRole("button", { name: "Request revision" }).click()
  expect((await revision).postDataJSON()).toEqual({ feedback: "Add reconnect coverage" })
})

test("web search and MCP settings use push-era product APIs", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "desktop", "settings mutation coverage runs once on desktop")
  await page.goto("/settings/web-search")
  await expect(page.getByRole("heading", { name: "Web Search" })).toBeVisible()
  await page.getByLabel("Allowed domains").fill("github.com\ndocs.rs")
  const save = page.waitForRequest((candidate) => candidate.url().endsWith("/settings/web-search") && candidate.method() === "PUT")
  await page.getByRole("button", { name: "Save changes" }).click()
  expect((await save).postDataJSON()).toMatchObject({ allowed_domains: ["github.com", "docs.rs"] })

  await page.goto("/settings/mcp")
  await expect(page.getByRole("heading", { name: "MCP Servers", exact: true })).toBeVisible()
  await expect(page.getByText("future_builtin", { exact: true })).toBeVisible()
  const recheck = page.waitForRequest((candidate) => candidate.url().endsWith("/mcp-servers/recheck") && candidate.method() === "POST")
  await page.getByRole("button", { name: "Recheck" }).click()
  await recheck
})

test("relay settings preserve blank secrets and submit replacements", async ({ page }, testInfo) => {
  await page.goto("/settings/github-app")
  await expect(page.getByText("Relay connection")).toBeVisible()

  const blankRelaySave = page.waitForRequest((candidate) => candidate.url().endsWith("/settings/relay") && candidate.method() === "PUT")
  await page.getByRole("button", { name: "Save relay" }).click()
  expect((await blankRelaySave).postDataJSON()).toEqual({
    enabled: true,
    url: "https://relay.example",
    node_id: "node-a",
  })

  const blankAppSave = page.waitForRequest((candidate) => candidate.url().endsWith("/settings/github-app") && candidate.method() === "PUT")
  await page.getByRole("button", { name: "Save app" }).click()
  expect((await blankAppSave).postDataJSON()).toEqual({
    public_url: "https://relay.example",
    base_url: "https://api.github.com",
    app_id: "123",
  })
  await expect(page.getByText("Verified identity")).toBeVisible()
  await expect(page.getByText("mai-team-app[bot]")).toBeVisible()
  await expect(page.getByText("283045312")).toBeVisible()
  await expect(page.getByRole("link", { name: "Manage on GitHub" })).toHaveAttribute(
    "href",
    "https://github.com/organizations/mai-team/settings/apps/mai-team-app",
  )
  const identityRefresh = page.waitForRequest((candidate) => candidate.url().endsWith("/settings/github-app/refresh") && candidate.method() === "POST")
  await page.getByRole("button", { name: "Refresh identity" }).click()
  await identityRefresh

  await page.getByLabel("Relay token").fill("new-relay-token")
  await page.getByLabel("PEM private key").fill("new-private-key")
  const relaySave = page.waitForRequest((candidate) => candidate.url().endsWith("/settings/relay") && candidate.method() === "PUT")
  await page.getByRole("button", { name: "Save relay" }).click()
  expect((await relaySave).postDataJSON()).toMatchObject({ token: "new-relay-token" })
  await expect(page.getByLabel("Relay token")).toHaveValue("")

  const appSave = page.waitForRequest((candidate) => candidate.url().endsWith("/settings/github-app") && candidate.method() === "PUT")
  await page.getByRole("button", { name: "Save app" }).click()
  expect((await appSave).postDataJSON()).toMatchObject({ private_key: "new-private-key" })
  await expect(page.getByLabel("PEM private key")).toHaveValue("")
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)).toBe(true)
  await expect(page.getByText("GitHub App settings saved")).toHaveCount(0, { timeout: 10_000 })
  await page.getByText("Verified identity").evaluate((element) => {
    element.scrollIntoView({ block: "start" })
    let scrollParent = element.parentElement
    while (scrollParent && scrollParent.scrollHeight <= scrollParent.clientHeight) {
      scrollParent = scrollParent.parentElement
    }
    scrollParent?.scrollBy({ top: -72 })
  })
  await expect(page).toHaveScreenshot(`github-app-settings-${testInfo.project.name}.png`, { animations: "disabled" })
})

async function installApiFixture(page: Page) {
  await page.route("**/*", async (route) => {
    const request = route.request()
    if (request.isNavigationRequest()) return route.continue()
    const url = new URL(request.url())
    const path = url.pathname
    if (path === "/events/product") return stream(route, ": connected\n\n")
    if (path === "/sessions/session-1/events") return stream(route, `event: snapshot\ndata: ${JSON.stringify(sessionFrame)}\n\n`)
    if (path === "/environments") return json(route, [environmentSummary])
    if (path === "/environments/env-1") return json(route, environmentDetail)
    if (path === "/providers") return json(route, providers)
    if (path === "/provider-catalog") return json(route, catalog)
    if (path === "/agent-config") return json(route, agentConfig)
    if (path === "/skills") return json(route, skills)
    if (path === "/projects") return json(route, [projectSummary])
    if (path === "/projects/project-1") return json(route, projectDetail)
    if (path === "/projects/project-1/review-jobs") return json(route, { jobs: [reviewJob, retryWaitingReviewJob, skippedReviewJob, changesRequestedReviewJob] })
    if (path === "/projects/project-1/review-jobs/job-1") return json(route, reviewJobDetail)
    if (path === "/projects/project-1/review-jobs/job-retry") return json(route, { ...retryWaitingReviewJob, attempts: [retryableReviewRun] })
    if (path === "/projects/project-1/review-jobs/job-skipped") return json(route, { ...skippedReviewJob, attempts: [] })
    if (path === "/projects/project-1/review-jobs/job-2") return json(route, { ...changesRequestedReviewJob, attempts: [changesRequestedReviewRun] })
    if (path === "/projects/project-1/review-runs") return json(route, { runs: [reviewRun, changesRequestedReviewRun] })
    if (path === "/projects/project-1/review-runs/review-1") return json(route, reviewRunDetail)
    if (path === "/projects/project-1/skills") return json(route, projectSkills)
    if (path === "/tasks") return json(route, [taskSummary])
    if (path === "/tasks/task-1") return json(route, taskDetail)
    if (path === "/settings/web-search") return json(route, webSearchSettings)
    if (path === "/relay/status") return json(route, relayStatus)
    if (path === "/settings/relay") return json(route, relaySettings)
    if (path === "/settings/github-app") return json(route, githubAppSettings)
    if (path === "/settings/github-app/refresh") return json(route, githubAppSettings)
    if (path === "/github/installations") return json(route, { installations: [] })
    if (path === "/relay/update") return json(route, relayUpdate)
    if (path === "/mcp-servers") return json(route, mcpServers)
    if (/^\/projects\/project-1\/pull-requests\/\d+\/review$/.test(path) && request.method() === "POST") return json(route, { queued: [1631], deduped: [], ignored: [], jobs: [reviewJob] })
    if (request.method() !== "GET" && (
      path.startsWith("/projects/project-1")
      || path.startsWith("/tasks/task-1")
      || path.startsWith("/mcp-servers")
      || path === "/settings/web-search"
    )) return json(route, {})
    if (path.includes("/messages") && request.method() === "POST") return json(route, { turn_id: "turn-next" })
    return route.continue()
  })
}

function json(route: Route, body: unknown) {
  return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(body) })
}

function stream(route: Route, body: string) {
  return route.fulfill({ status: 200, contentType: "text/event-stream", headers: { "cache-control": "no-cache" }, body })
}

const usage = { input_tokens: 1200, cached_input_tokens: 400, output_tokens: 180, reasoning_output_tokens: 80, total_tokens: 1380 }
const relayStatus = { enabled: true, connected: true, relay_url: "https://relay.example", node_id: "node-a" }
const relaySettings = { enabled: true, url: "https://relay.example", has_token: true, node_id: "node-a" }
const githubAppSettings = {
  app_id: "123",
  base_url: "https://api.github.com",
  public_url: "https://relay.example",
  has_private_key: true,
  github_name: "Mai Team",
  app_slug: "mai-team-app",
  app_html_url: "https://github.com/apps/mai-team-app",
  owner_login: "mai-team",
  owner_type: "Organization",
  bot_login: "mai-team-app[bot]",
  bot_user_id: 283045312,
  install_url: "https://github.com/apps/mai-team-app/installations/select_target",
  manage_url: "https://github.com/organizations/mai-team/settings/apps/mai-team-app",
}
const relayUpdate = {
  current_version: "0.1.0",
  latest_version: "0.1.0",
  has_update: false,
  can_update: true,
}
const agent = {
  id: "agent-1", name: "Review workspace", role: "executor", state: { resource: "ready", runtime: { lifecycle: "active", activity: "idle", active_turn: null, pending_inputs: 0 } },
  provider_id: "future-provider", provider_name: "Future Cloud", model: "future-model", reasoning_effort: "balanced", created_at: "2026-07-20T00:00:00Z", updated_at: "2026-07-20T00:00:00Z", token_usage: usage,
}
const session = { id: "session-1", title: "Review", created_at: "2026-07-20T00:00:00Z", updated_at: "2026-07-20T00:00:00Z", message_count: 2, token_usage: usage }
const rootAgent = { ...agent, sessions: [session], selected_session_id: "session-1" }
const environmentSummary = { id: "env-1", name: "TGOS review", status: "ready", root_agent: agent }
const environmentDetail = { ...environmentSummary, root_agent: rootAgent, conversations: [session], selected_conversation_id: "session-1" }

const sessionFrame = { type: "snapshot", snapshot: {
  schemaVersion: 1, sessionId: "session-1", throughSequence: 6,
  messages: [
    { messageId: "message-1", sessionId: "session-1", turnId: "turn-1", role: "user", status: "completed", createdAt: 1, updatedAt: 1 },
    { messageId: "message-2", sessionId: "session-1", turnId: "turn-1", role: "assistant", status: "completed", createdAt: 2, updatedAt: 3 },
  ],
  parts: [
    { partId: "part-1", messageId: "message-1", sessionId: "session-1", turnId: "turn-1", order: 0, revision: 0, status: "completed", createdAt: 1, updatedAt: 1, content: { type: "text", channel: "user", text: "Inspect the canonical session stream" } },
    { partId: "part-search", messageId: "message-2", sessionId: "session-1", turnId: "turn-1", order: 0, revision: 1, status: "completed", createdAt: 1_000, updatedAt: 4_000, completedAt: 4_000, content: { type: "tool", tool: { toolCallId: "tool-call-search", name: "search_files", arguments: JSON.stringify({ query: "session timeline", path: "src/features/session" }), result: JSON.stringify({ query: "session timeline", count: 12, files: [{ path: "src/features/session/session-timeline.tsx", matches: [{ line: 15, text: "export function SessionTimeline" }] }] }), activityGroupId: "turn-1:tools" } } },
    { partId: "part-read", messageId: "message-2", sessionId: "session-1", turnId: "turn-1", order: 1, revision: 1, status: "completed", createdAt: 4_000, updatedAt: 7_000, completedAt: 7_000, content: { type: "tool", tool: { toolCallId: "tool-call-read", name: "read_file", arguments: JSON.stringify({ path: "src/features/session/session-timeline.tsx" }), result: JSON.stringify({ path: "src/features/session/session-timeline.tsx", startLine: 1, endLine: 90, text: "export function SessionTimeline() {}" }), activityGroupId: "turn-1:tools" } } },
    { partId: "part-tool", messageId: "message-2", sessionId: "session-1", turnId: "turn-1", order: 2, revision: 1, status: "completed", createdAt: 7_000, updatedAt: 15_000, completedAt: 15_000, content: { type: "tool", tool: { toolCallId: "tool-call-1", name: "exec", arguments: JSON.stringify({ command: "cargo test -p mai-server projects::review", cwd: "/workspace/mai-team" }), result: JSON.stringify({ status: "completed", exitCode: 0, timedOut: false, stdout: "Finished focused review checks", stderr: "", outputFile: ".mai/tool-output/tool-call-1.log" }), exitCode: 0, workingDirectory: "/workspace/mai-team", activityGroupId: "turn-1:tools" } } },
    { partId: "part-patch", messageId: "message-2", sessionId: "session-1", turnId: "turn-1", order: 3, revision: 1, status: "completed", createdAt: 15_000, updatedAt: 19_000, completedAt: 19_000, content: { type: "tool", tool: { toolCallId: "tool-call-patch", name: "apply_patch", arguments: JSON.stringify({ patch: "*** Begin Patch\n*** Update File: src/features/session/session-timeline.tsx\n*** Update File: src/features/session/tool-call-details.tsx\n*** End Patch" }), result: JSON.stringify({ changedFiles: ["src/features/session/session-timeline.tsx", "src/features/session/tool-call-details.tsx"] }), activityGroupId: "turn-1:tools" } } },
    { partId: "part-2", messageId: "message-2", sessionId: "session-1", turnId: "turn-1", order: 4, revision: 0, status: "completed", createdAt: 19_000, updatedAt: 19_000, completedAt: 19_000, content: { type: "text", channel: "final", text: "The unified event channel is active and the review workspace is ready." } },
  ],
  interactions: [], agents: [],
  timelineEvents: [
    { eventId: "todo-latest", sessionId: "session-1", sequence: 5, createdAt: 5, kind: { type: "todoListChanged", snapshot: { callId: "todo-call-latest", explanation: "Review flow", items: [{ step: "Read project constraints", status: "completed" }, { step: "Inspect PR changes", status: "inProgress" }, { step: "Run focused checks", status: "pending" }] } } },
    { eventId: "activity-1", sessionId: "session-1", sequence: 6, createdAt: 6, kind: { type: "subAgentActivity", callId: "agent-call", kind: "spawned" } },
    { eventId: "todo-old", sessionId: "session-1", sequence: 3, createdAt: 3, kind: { type: "todoListChanged", snapshot: { callId: "todo-call-old", explanation: "Old flow", items: [{ step: "Outdated task", status: "pending" }] } } },
  ],
  runtime: { sessionId: "session-1", usage: { model: "future-model", contextWindow: 200000, latestContextTokens: 1200, promptTokens: 1200, completionTokens: 180, cachedPromptTokens: 400, totalTokens: 1380, estimatedCosts: [], hasUnpricedUsage: false, updatedAt: 3 }, activeSkills: ["review-helper"], activeMcpServers: [], activeLspServers: [], agentCount: 1, updatedAt: 3 },
  activatedSkills: [], planEvents: [],
} }

const catalog = { schema_version: 4, revision: "future", presets: [{ id: "future-provider", display_name: "Future Cloud", description: "Future-compatible Responses provider", base_url: "https://future.invalid/v1", suggested_model: "future-model", model_catalog_id: "future-catalog", icon_key: "future", credential: { label: "Future token", env_var: "FUTURE_TOKEN" }, transport: { protocol: "responses", connection_modes: [{ id: "web_socket", display_name: "WebSocket" }, { id: "http", display_name: "HTTP" }], default_connection_mode: "web_socket" } }], model_catalogs: { "future-catalog": { models: [{ id: "future-model", display_name: "Future Model", reasoning: { default_variant: "balanced", variants: [{ id: "eco", label: "Eco" }, { id: "balanced", label: "Balanced" }] } }] } } }
const providers = { providers: [{ id: "future-provider", name: "Future Cloud", preset_id: "future-provider", base_url: "https://future.invalid/v1", enabled: true, default_model: "future-model", models: catalog.model_catalogs["future-catalog"].models, has_api_key: true, transport: { protocol: "responses", connection_mode: "web_socket", connection_modes: catalog.presets[0].transport.connection_modes }, capability_selection: { source: "preset_defaults" }, service_capabilities: { web_search: { hosted_responses: true } }, catalog: { source: "bundled", catalog_id: "future-catalog", additional_models: [] } }], default_provider_id: "future-provider" }
const agentConfig = { planner: null, explorer: null, executor: null, reviewer: null, effective_planner: { provider_id: "future-provider", provider_name: "Future Cloud", model: "future-model" }, effective_explorer: { provider_id: "future-provider", provider_name: "Future Cloud", model: "future-model" }, effective_executor: { provider_id: "future-provider", provider_name: "Future Cloud", model: "future-model" }, effective_reviewer: { provider_id: "future-provider", provider_name: "Future Cloud", model: "future-model" }, validation_error: null }
const skills = { roots: ["/skills"], skills: [{ name: "review-helper", description: "Review project changes", path: "/skills/review-helper/SKILL.md", scope: "system", enabled: true }], errors: [] }

const projectAgent = {
  ...rootAgent,
  id: "project-agent-1",
  name: "TGOS Maintainer",
  role: "maintainer",
  project_id: "project-1",
}
const reviewRun = {
  id: "review-1",
  job_id: "job-1",
  attempt_index: 1,
  status: "succeeded",
  pr: 1631,
  summary: "Canonical session events are correctly isolated.",
  started_at: "2026-07-20T00:00:00Z",
  finished_at: "2026-07-20T00:05:00Z",
  outcome: "review_submitted",
  review_event: "approve",
  reviewer_agent_id: "reviewer-1",
  turn_id: "review-turn-1",
  token_usage: usage,
}
const changesRequestedReviewRun = {
  ...reviewRun,
  id: "review-2",
  job_id: "job-2",
  pr: 1563,
  summary: "Requested changes for a release blocker.",
  review_event: "request_changes",
  started_at: "2026-07-19T23:50:00Z",
  finished_at: "2026-07-19T23:58:00Z",
}
const retryableReviewRun = {
  ...reviewRun,
  id: "review-retry-1",
  job_id: "job-retry",
  pr: 1665,
  status: "retryable_failed",
  outcome: "failed",
  review_event: null,
  summary: "The provider was temporarily overloaded; the same reviewer session will continue.",
  error: "The server is overloaded",
  failure: { category: "provider_capacity", code: "server_is_overloaded", http_status: 503, message: "The server is overloaded", retry: { retryable: { retry_after_ms: 30000 } } },
}
const reviewJob = {
  id: "job-1",
  project_id: "project-1",
  pr: 1631,
  head_sha: "abc123abc123abc123abc123abc123abc123abc1",
  source: "automatic",
  reason: "eligible pull request",
  status: "succeeded",
  attempt_count: 1,
  max_attempts: 5,
  reviewer_agent_id: "reviewer-1",
  submission_intent: { job_id: "job-1", head_sha: "abc123abc123abc123abc123abc123abc123abc1", event: "approve", body_hash: "sha256:review-body", comment_count: 0, created_at: "2026-07-20T00:03:30Z" },
  submission_receipt: { github_review_id: 4512, event: "approve", head_sha: "abc123abc123abc123abc123abc123abc123abc1", html_url: "https://github.com/rcore-os/tgoskits/pull/1631#pullrequestreview-4512", submitted_at: "2026-07-20T00:04:00Z" },
  created_at: "2026-07-20T00:00:00Z",
  updated_at: "2026-07-20T00:05:00Z",
  finished_at: "2026-07-20T00:05:00Z",
}
const changesRequestedReviewJob = {
  ...reviewJob,
  id: "job-2",
  pr: 1563,
  head_sha: "def456def456def456def456def456def456def4",
  attempt_count: 1,
  submission_intent: { ...reviewJob.submission_intent, job_id: "job-2", head_sha: "def456def456def456def456def456def456def4", event: "request_changes", comment_count: 2 },
  submission_receipt: { ...reviewJob.submission_receipt, github_review_id: 4511, event: "request_changes", head_sha: "def456def456def456def456def456def456def4" },
  created_at: "2026-07-19T23:50:00Z",
  updated_at: "2026-07-19T23:58:00Z",
  finished_at: "2026-07-19T23:58:00Z",
}
const retryWaitingReviewJob = {
  ...reviewJob,
  id: "job-retry",
  pr: 1665,
  head_sha: "fedcba9876543210fedcba9876543210fedcba98",
  source: "webhook",
  reason: "pull request synchronized",
  status: "retry_waiting",
  attempt_count: 1,
  next_attempt_at: "2026-07-20T00:06:00Z",
  failure: { category: "provider_capacity", code: "server_is_overloaded", http_status: 503, message: "The server is overloaded", retry: { retryable: { retry_after_ms: 30000 } } },
  submission_intent: null,
  submission_receipt: null,
  created_at: "2026-07-20T00:01:00Z",
  updated_at: "2026-07-20T00:05:30Z",
  finished_at: null,
}
const skippedReviewJob = {
  ...reviewJob,
  id: "job-skipped",
  pr: 1664,
  head_sha: "beadbeadbeadbeadbeadbeadbeadbeadbeadbead",
  source: "webhook",
  reason: "check_run",
  status: "skipped",
  attempt_count: 1,
  reviewer_agent_id: null,
  skip_reason: "already_reviewed_current_head",
  submission_intent: null,
  submission_receipt: null,
  created_at: "2026-07-20T00:02:00Z",
  updated_at: "2026-07-20T00:02:01Z",
  finished_at: "2026-07-20T00:02:01Z",
}
const reviewJobDetail = { ...reviewJob, attempts: [reviewRun] }
const projectSummary = {
  id: "project-1",
  name: "TGOS Kits",
  repository: "https://github.com/rcore-os/tgoskits.git",
  repository_full_name: "rcore-os/tgoskits",
  status: "ready",
  maintainer_agent_id: projectAgent.id,
}
const projectDetail = {
  ...projectSummary,
  branch: "main",
  clone_status: "ready",
  auth_status: "ready",
  auto_review_enabled: false,
  docker_image: "ubuntu:24.04",
  reviewer_extra_prompt: "Read project constraints first.",
  maintainer_agent: projectAgent,
  agents: [projectAgent],
  selected_agent_id: projectAgent.id,
  selected_agent: projectAgent,
  review_runs: [reviewRun, changesRequestedReviewRun],
}
const reviewExecResult = JSON.stringify({ status: "completed", exitCode: 0, timedOut: false, stdout: "All reviewer checks passed", stderr: "", outputFile: ".mai/tool-output/review-exec.log" })
const reviewNoteResult = JSON.stringify({ revision: 4, contentHash: "sha256:notes", totalBytes: 168, totalLines: 9, startLine: 1, endLine: 9, text: "## F-1\n\n**Status:** resolved\n\nThe focused test disproved the initial concern." })
const reviewGithubArguments = JSON.stringify({ method: "POST", path: "/repos/rcore-os/tgoskits/pulls/1631/reviews", body: { event: "APPROVE", body: "Focused checks passed; approving this change.", commit_id: "abc123" } })
const reviewGithubResult = JSON.stringify({ id: 4512, state: "APPROVED", submitted_at: "2026-07-20T00:04:00Z", html_url: "https://github.com/rcore-os/tgoskits/pull/1631#pullrequestreview-4512", body: "Focused checks passed; approving this change.", user: { login: "reviewer" }, pull_request_url: "https://api.github.com/repos/rcore-os/tgoskits/pulls/1631", repository: { omitted: "large nested response" } })
const reviewRunDetail = {
  ...reviewRun,
  messages: [
    { role: "assistant", content: "Reviewed the changed files and ran focused validation.", created_at: "2026-07-20T00:01:00Z" },
    { role: "tool", content: reviewExecResult, created_at: "2026-07-20T00:02:00Z" },
    { role: "tool", content: reviewNoteResult, created_at: "2026-07-20T00:03:00Z" },
    { role: "tool", content: reviewGithubResult, created_at: "2026-07-20T00:04:00Z" },
    { role: "assistant", content: JSON.stringify({ outcome: "review_submitted", review_event: "approve", summary: "All checks passed and the review was submitted atomically." }), created_at: "2026-07-20T00:05:00Z" },
  ],
  events: [
    reviewToolEvent("review-exec", 1, 1, "exec", JSON.stringify({ command: "cargo test -p mai-server projects::review", cwd: "/workspace/mai-team" }), reviewExecResult),
    reviewToolEvent("review-note", 0, 2, "read_session_note", JSON.stringify({ startLine: 1, maxLines: 40, expectedRevision: 3 }), JSON.stringify({ revision: 2, totalLines: 3, text: "Outdated note" })),
    reviewToolEvent("review-note", 2, 3, "read_session_note", JSON.stringify({ startLine: 1, maxLines: 40, expectedRevision: 4 }), reviewNoteResult),
    reviewToolEvent("review-github", 1, 4, "github_api_request", reviewGithubArguments, reviewGithubResult),
  ],
}

function reviewToolEvent(partId: string, revision: number, sequence: number, name: string, argumentsValue: string, result: string) {
  return {
    eventId: `review-event-${sequence}`,
    sessionId: "session-1",
    sourceAgentId: "reviewer-1",
    turnId: "review-turn-1",
    emittedAt: sequence,
    position: { persistence: "durable", sequence },
    kind: { type: "partChanged", part: {
      partId,
      messageId: `review-message-${partId}`,
      sessionId: "session-1",
      turnId: "review-turn-1",
      order: sequence,
      revision,
      status: "completed",
      createdAt: sequence,
      updatedAt: sequence,
      content: { type: "tool", tool: { toolCallId: `${partId}-call`, name, arguments: argumentsValue, result } },
    } },
  }
}
const projectSkills = {
  roots: ["/project/repo/.agents/skills"],
  skills: [{ name: "review-helper", description: "Read default-branch constraints", path: "/project/repo/.agents/skills/review-helper/SKILL.md" }],
  errors: [],
}

const taskAgent = {
  ...rootAgent,
  id: "task-agent-1",
  name: "Planner",
  role: "planner",
  task_id: "task-1",
}
const taskSummary = {
  id: "task-1",
  title: "Unify session events",
  status: "awaiting_approval",
  plan_status: "ready",
  plan_version: 2,
  planner_agent_id: taskAgent.id,
  current_agent_id: taskAgent.id,
  agent_count: 1,
  review_rounds: 0,
  created_at: "2026-07-20T00:00:00Z",
  updated_at: "2026-07-20T00:01:00Z",
}
const taskDetail = {
  ...taskSummary,
  plan: {
    status: "ready",
    version: 2,
    title: "Implement canonical reducer",
    markdown: "## Implement canonical reducer\n\nUse one session event protocol.",
    saved_at: "2026-07-20T00:01:00Z",
  },
  plan_history: [],
  reviews: [],
  agents: [taskAgent],
  selected_agent_id: taskAgent.id,
  selected_agent: taskAgent,
  artifacts: [{ id: "artifact-1", name: "event-contract.json", size_bytes: 512, created_at: "2026-07-20T00:02:00Z" }],
}

const webSearchSettings = {
  config: { mode: "cached", context_size: "medium", allowed_domains: [], location: null },
  roles: {
    reviewer: {
      configured_mode: "cached",
      effective_mode: "cached",
      availability: "available",
      path: "standalone",
      provider_id: "future-provider",
      model: "future-model",
    },
  },
}
const mcpServers = {
  servers: [{
    descriptor: {
      id: "future_builtin",
      source: "builtin",
      transport: "streamable_http",
      endpoint: "https://future.invalid/mcp",
      builtIn: true,
    },
    enabled: true,
    availability: "available",
    ready_agents: 1,
    failed_agents: 0,
    checking_agents: 0,
    total_agents: 1,
    tool_count: 4,
    config: null,
  }],
}
