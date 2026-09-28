import { expect, test, type Page, type Route } from "@playwright/test"

test.beforeEach(async ({ page }) => {
  const updates = { "thread-a": snapshotUpdate("thread-a"), "thread-b": snapshotUpdate("thread-b"), "thread-c": activeSnapshotUpdate("thread-c") }
  await installThreadStreamFixture(page, updates)
  await installApiFixture(page)
})

async function installThreadStreamFixture(page: Page, updates: Record<string, unknown>) {
  await page.addInitScript(({ updates }) => {
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
          const update = Object.entries(updates).find(([threadId]) => this.url.includes(`/threads/${threadId}/events`))?.[1]
          if (update) this.dispatchEvent(new MessageEvent("snapshot", { data: JSON.stringify(update) }))
        }, 0)
      }

      close() {
        this.readyState = FixtureEventSource.CLOSED
      }
    }
    Object.defineProperty(window, "EventSource", { configurable: true, value: FixtureEventSource })
  }, { updates })
}

test("Thread 切换使用隔离 store，消息发送到目标 Thread", async ({ page }) => {
  await page.goto("/chat/env-a")
  await expect(page.getByText("Alpha message")).toBeVisible()
  await expect(page.getByText("Beta message")).toHaveCount(0)

  if ((page.viewportSize()?.width ?? 0) < 1024) {
    await page.getByRole("button", { name: "Open Chat" }).click()
  }
  await page.getByRole("button", { name: /Environment B/ }).click()
  await expect(page).toHaveURL(/\/chat\/env-b$/)
  await expect(page.getByText("Beta message")).toBeVisible()
  await expect(page.getByText("Alpha message")).toHaveCount(0)

  await page.getByRole("button", { name: "Select skills" }).click()
  await page.getByRole("menuitemcheckbox", { name: /project-review/ }).click()
  await page.keyboard.press("Escape")
  await page.getByPlaceholder(/Send a command/).fill("Continue B")
  const request = page.waitForRequest((candidate) => candidate.url().endsWith("/threads/thread-b/messages") && candidate.method() === "POST")
  await page.getByRole("button", { name: /Send/ }).click()
  expect((await request).postDataJSON()).toMatchObject({ message: "Continue B", skill_mentions: ["project-review"] })
})

test("Thread timeline 在全部视口可用", async ({ page }) => {
  const consoleProblems: string[] = []
  page.on("console", (message) => {
    if (message.type() === "error" || message.type() === "warning") consoleProblems.push(message.text())
  })
  await page.goto("/chat/env-a")
  await expect(page.getByText("Alpha message")).toBeVisible()
  await expect(page.getByRole("article", { name: "Skill loaded: project-review" })).toBeVisible()
  await expect(page.getByRole("feed", { name: "Conversation timeline" })).toBeVisible()
  await expect(page.getByRole("article", { name: "Mai Team response" })).toContainText("Alpha message")
  await expect(page.locator("strong").filter({ hasText: "future-model" })).toBeVisible()
  await expect(page.getByText("Tokens 1,380")).toBeVisible()
  await expect(page.getByText("Context 1.2K / 128.0K")).toBeVisible()

  // 历史来自 `/threads/{id}/turns`：首帧不含 items，更早的 Turn 通过 cursor 分页加载。
  await page.getByRole("button", { name: "Load earlier history" }).click()
  await expect(page.getByText("Earlier thread-a context")).toBeVisible()
  await expect(page.getByRole("button", { name: "Load earlier history" })).toHaveCount(0)

  const skills = page.getByRole("button", { name: "2 skills loaded" })
  await expect(skills).toBeVisible()
  if ((page.viewportSize()?.width ?? 0) >= 1024) {
    await skills.hover()
    await expect(page.getByRole("tooltip")).toContainText("project-review, rust-code-quality")
  }
  await skills.click()
  await expect(page.getByRole("menuitemcheckbox", { name: "project-review" })).toHaveAttribute("aria-checked", "true")
  await expect(page.getByRole("menuitemcheckbox", { name: "rust-code-quality" })).toHaveAttribute("aria-checked", "true")
  await expect(page.getByRole("tooltip")).toHaveCount(0)
  await page.keyboard.press("Escape")
  await expect(page.getByRole("menuitemcheckbox", { name: "project-review" })).toHaveCount(0)
  await page.getByRole("article", { name: "Mai Team response" }).click()
  await expect(page.getByRole("tooltip")).toHaveCount(0)
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)).toBe(true)
  expect(consoleProblems).toEqual([])
})

test("运行中 Turn 显示已提交历史和活动状态", async ({ page }) => {
  await page.goto("/chat/env-c")
  await expect(page.getByRole("article", { name: "You message" })).toContainText("Review current pull request")
  await expect(page.getByRole("feed", { name: "Conversation timeline" })).toContainText("Thinking")
})

async function installApiFixture(page: Page) {
  await page.route("**/*", async (route) => {
    const request = route.request()
    if (request.isNavigationRequest()) return route.continue()
    const path = new URL(request.url()).pathname
    if (path === "/environments") return json(route, [environment("env-a", "Environment A", "thread-a"), environment("env-b", "Environment B", "thread-b")])
    if (path === "/environments/env-a") return json(route, environment("env-a", "Environment A", "thread-a"))
    if (path === "/environments/env-b") return json(route, environment("env-b", "Environment B", "thread-b"))
    if (path === "/environments/env-c") return json(route, environment("env-c", "Environment C", "thread-c"))
    if (path === "/providers") return json(route, providerFixture())
    if (path === "/skills") return json(route, {
      skills: [{ name: "project-review", description: "Review project changes", path: "/skills/project-review/SKILL.md", scope: "project", enabled: true }],
      roots: ["/skills"],
      errors: [],
    })
    const turns = path.match(/^\/threads\/([^/]+)\/turns$/)
    if (turns) return json(route, threadTurnPage(decodeURIComponent(turns[1]), new URL(request.url()).searchParams.get("cursor")))
    if (path === "/threads/thread-c/active-turn") return json(route, activeTurnHistory())
    if (path.startsWith("/threads/") && path.endsWith("/messages") && request.method() === "POST") return json(route, { turn_id: "turn-next" })
    return route.continue()
  })
}

function providerFixture() {
  return {
    providers: [{
      id: "future-provider",
      config: {
        preset: "future-provider",
        name: "Future Cloud",
        base_url: "https://future.invalid/v1",
        capabilities: {},
        catalog: {},
      },
      models: [{
        slug: "future-model",
        display_name: "Future Model",
        parameters: [{ name: "effort", candidates: ["balanced", "max"] }],
        transport: {
          protocol: "responses",
          supported_connection_modes: ["http"],
          default_connection_mode: "http",
        },
      }],
      has_api_key: true,
      has_http_headers: false,
    }],
  }
}

function json(route: Route, body: unknown) {
  return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(body) })
}

function environment(id: string, name: string, threadId: string) {
  const agent = {
    id: threadId,
    name: `${name} Agent`,
    role: "planner",
    resource: { state: "ready", error: null },
    runtime: threadSnapshot(threadId, name),
    usage: usageSnapshot(),
    provider_id: "future-provider",
    provider_name: "Future Cloud",
    model: "future-model",
    created_at: "2026-08-11T00:00:00Z",
    updated_at: "2026-08-11T00:00:00Z",
    thread: thread(threadId, name),
  }
  return { id, name, status: "executing", root_agent_id: agent.id, thread_count: 1, docker_image: "ubuntu:24.04", created_at: agent.created_at, updated_at: agent.updated_at, root_agent: agent }
}

// Thread 首帧只携带 Thread 级状态；历史 Turn/items 由 `/threads/{id}/turns` 提供。
function snapshotUpdate(threadId: string) {
  return {
    type: "snapshot",
    snapshot: threadSnapshot(threadId, threadId),
  }
}

function activeSnapshotUpdate(threadId: string) {
  return {
    type: "snapshot",
    snapshot: {
      ...threadSnapshot(threadId, threadId),
      activeTurn: { id: `${threadId}:running`, threadId, revision: 1, state: { kind: "running", data: { startedAt: 1, phase: "thinking" } }, updatedAt: 1 },
    },
  }
}

function activeTurnHistory() {
  const threadId = "thread-c"
  const turnId = `${threadId}:running`
  return {
    turn: { id: turnId, threadId, revision: 1, state: { kind: "running", data: { startedAt: 1, phase: "thinking" } }, updatedAt: 1 },
    contextDisposition: "active",
    items: [{ id: `${threadId}:user`, threadId, turnId, ordinal: 1, revision: 0, createdAt: 1, updatedAt: 1, state: { kind: "text", data: { channel: "user", text: "Review current pull request", lifecycle: { kind: "completed", data: { completedAt: 1 } } } } }],
  }
}

const THREAD_TEXTS: Record<string, string> = { "thread-a": "Alpha message", "thread-b": "Beta message" }

// `/threads/{id}/turns` 的一页：Turn 终态从新到旧返回，`nextCursor` 指向更早的一页。
function threadTurnPage(threadId: string, cursor: string | null) {
  const text = THREAD_TEXTS[threadId]
  if (!text) return { turns: [] }
  if (cursor) return { turns: [olderTurnHistory(threadId)] }
  return {
    turns: [currentTurnHistory(threadId, text)],
    nextCursor: threadId === "thread-a" ? "older:thread-a" : undefined,
  }
}

function currentTurnHistory(threadId: string, text: string) {
  const turnId = `${threadId}:turn`
  return { turn: completedTurn(threadId, turnId, 15), contextDisposition: "active", items: [
    { id: `${threadId}:user`, threadId, turnId, ordinal: 1, revision: 0, createdAt: 1, updatedAt: 1, state: { kind: "text", data: { channel: "user", text: "Review the Rust service for correctness and maintainability.", lifecycle: { kind: "completed", data: { completedAt: 1 } } } } },
    { id: `${threadId}:reasoning`, threadId, turnId, ordinal: 2, revision: 0, createdAt: 2, updatedAt: 8, state: { kind: "thinking", data: { summary: ["Inspecting project structure and dependencies"], content: ["I will focus on ownership boundaries and error paths."], lifecycle: { kind: "completed", data: { completedAt: 8 } } } } },
    { id: `${threadId}:tool`, threadId, turnId, ordinal: 3, revision: 0, createdAt: 9, updatedAt: 12, state: { kind: "tool", data: { invocation: { toolCallId: `${threadId}:read`, name: "read_file", arguments: JSON.stringify({ path: "src/runtime.rs" }) }, state: { kind: "succeeded", data: { completedAt: 12, output: { result: "runtime source", exitCode: 0 } } } } } },
    { id: `${threadId}:skill`, threadId, turnId, ordinal: 4, revision: 0, createdAt: 13, updatedAt: 13, state: { kind: "skill", data: { activation: { name: "project-review", source: "project", providerId: "fixture", resourceBase: { kind: "directory", path: "/skills/project-review" }, turnId, cause: { kind: "tool", toolCallId: `${threadId}:skill-view` }, activatedAt: 13 } } } },
    { id: `${threadId}:commentary`, threadId, turnId, ordinal: 5, revision: 0, createdAt: 14, updatedAt: 14, state: { kind: "text", data: { channel: "commentary", text: "Checked the runtime boundary and focused the review on actionable findings.", lifecycle: { kind: "completed", data: { completedAt: 14 } } } } },
    { id: `${threadId}:item`, threadId, turnId, ordinal: 6, revision: 0, createdAt: 15, updatedAt: 15, state: { kind: "text", data: { channel: "final", text: `## Rust service review\n\n${text}\n\nThe implementation is sound overall.\n\n- **Correctness:** ownership is explicit.\n- **Testing:** add a regression for the failure path.`, lifecycle: { kind: "completed", data: { completedAt: 15 } } } } },
  ] }
}

function olderTurnHistory(threadId: string) {
  const turnId = `${threadId}:turn-older`
  return { turn: completedTurn(threadId, turnId, 1), contextDisposition: "active", items: [
    { id: `${threadId}:older:user`, threadId, turnId, ordinal: 1, revision: 0, createdAt: 1, updatedAt: 1, state: { kind: "text", data: { channel: "user", text: `Earlier ${threadId} context`, lifecycle: { kind: "completed", data: { completedAt: 1 } } } } },
  ] }
}

function completedTurn(threadId: string, turnId: string, updatedAt: number) {
  return { id: turnId, threadId, revision: 1, state: { kind: "completed", data: { startedAt: null, completedAt: updatedAt, completion: "normal" } }, updatedAt }
}

function usageSnapshot() {
  return { hasIncompleteUsage: false, model: "future-model", contextWindow: 128000, latestContextTokens: 1200, promptTokens: 1200, completionTokens: 180, cachedPromptTokens: 400, cacheWriteTokens: 0, cacheMissTokens: 800, reasoningTokens: 80, inferenceCount: 1, totalTokens: 1380, hasUnpricedUsage: false, updatedAt: 1 }
}

function threadSnapshot(threadId: string, title: string) {
  return {
    schemaVersion: 7,
    revision: 1,
    thread: thread(threadId, title),
    interactions: [],
    runtime: { threadId, usage: usageSnapshot(), activeSkills: ["project-review", "rust-code-quality"], activeMcpServers: [], activeLspServers: [], updatedAt: 2 },
  }
}

function thread(id: string, title: string) {
  return { id, projectId: "", title, mode: "mode.simple", workspaceMode: "local", workspacePath: "/workspace", rootThreadId: id, role: "planner", agentPath: "root", status: "idle", createdAt: 1, updatedAt: 1, archived: false }
}
