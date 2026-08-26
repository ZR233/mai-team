import { expect, test, type Page, type Route } from "@playwright/test"

test.beforeEach(async ({ page }) => {
  const updates = { "thread-a": snapshotUpdate("thread-a", "Alpha message"), "thread-b": snapshotUpdate("thread-b", "Beta message") }
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
  await page.goto("/chat/env-a")
  await expect(page.getByText("Alpha message")).toBeVisible()
  await expect(page.locator("strong").filter({ hasText: "future-model" })).toBeVisible()
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)).toBe(true)
})

async function installApiFixture(page: Page) {
  await page.route("**/*", async (route) => {
    const request = route.request()
    if (request.isNavigationRequest()) return route.continue()
    const path = new URL(request.url()).pathname
    if (path === "/environments") return json(route, [environment("env-a", "Environment A", "thread-a"), environment("env-b", "Environment B", "thread-b")])
    if (path === "/environments/env-a") return json(route, environment("env-a", "Environment A", "thread-a"))
    if (path === "/environments/env-b") return json(route, environment("env-b", "Environment B", "thread-b"))
    if (path === "/providers") return json(route, providerFixture())
    if (path === "/skills") return json(route, {
      skills: [{ name: "project-review", description: "Review project changes", path: "/skills/project-review/SKILL.md", scope: "project", enabled: true }],
      roots: ["/skills"],
      errors: [],
    })
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

const usage = { input_tokens: 1200, cached_input_tokens: 400, output_tokens: 180, reasoning_output_tokens: 80, total_tokens: 1380 }

function environment(id: string, name: string, threadId: string) {
  const agent = {
    id: threadId,
    name: `${name} Agent`,
    role: "planner",
    resource: { state: "ready", error: null },
    runtime: {
      identity: { id: threadId, parentId: null, role: "planner", depth: 0 },
      state: { kind: "idle", data: null },
      pendingInputs: 0,
      progress: null,
      lastTurn: null,
      revision: 1,
      eventSequence: 1,
      updatedAt: 1,
    },
    provider_id: "future-provider",
    provider_name: "Future Cloud",
    model: "future-model",
    created_at: "2026-08-11T00:00:00Z",
    updated_at: "2026-08-11T00:00:00Z",
    token_usage: usage,
    thread: thread(threadId, name),
  }
  return { id, name, status: "executing", root_agent_id: agent.id, thread_count: 1, docker_image: "ubuntu:24.04", created_at: agent.created_at, updated_at: agent.updated_at, root_agent: agent }
}

function snapshotUpdate(threadId: string, text: string) {
  return {
    type: "snapshot",
    snapshot: {
      schemaVersion: 1,
      revision: 1,
      thread: thread(threadId, threadId),
      items: [{ id: `${threadId}:item`, threadId, turnId: `${threadId}:turn`, ordinal: 0, revision: 0, status: "completed", createdAt: 1, updatedAt: 1, completedAt: 1, content: { type: "agentMessage", channel: "final", text } }],
      interactions: [],
      runtime: { threadId, usage: usageSnapshot(threadId), activeSkills: [], activeMcpServers: [], activeLspServers: [], updatedAt: 1 },
    },
  }
}

function usageSnapshot(threadId: string) {
  return { threadId, model: "future-model", latestContextTokens: 1200, promptTokens: 1200, completionTokens: 180, cachedPromptTokens: 400, cacheWriteTokens: 0, cacheMissTokens: 800, reasoningTokens: 80, inferenceCount: 1, totalTokens: 1380, hasUnpricedUsage: false, updatedAt: 1 }
}

function thread(id: string, title: string) {
  return { id, projectId: "", title, mode: "simple", rootThreadId: id, role: "planner", agentPath: "root", status: "idle", createdAt: 1, updatedAt: 1, archived: false }
}
