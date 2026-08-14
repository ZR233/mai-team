import { expect, test, type Page, type Route } from "@playwright/test"

test.beforeEach(async ({ page }) => {
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
  }, { updates: { "thread-a": snapshotUpdate("thread-a", "Alpha message"), "thread-b": snapshotUpdate("thread-b", "Beta message") } })
  await installApiFixture(page)
})

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

  await page.getByPlaceholder(/Send a command/).fill("Continue B")
  const request = page.waitForRequest((candidate) => candidate.url().endsWith("/threads/thread-b/messages") && candidate.method() === "POST")
  await page.getByRole("button", { name: /Send/ }).click()
  expect((await request).postDataJSON()).toMatchObject({ message: "Continue B", skill_mentions: [] })
})

test("Thread timeline 在全部视口可用", async ({ page }) => {
  await page.goto("/chat/env-a")
  await expect(page.getByText("Alpha message")).toBeVisible()
  await expect(page.locator("strong").filter({ hasText: "future-model" })).toBeVisible()
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)).toBe(true)
})

test("连续工具调用折叠为分组，思考默认收起", async ({ page }) => {
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
  }, { updates: { "thread-a": groupedSnapshot() } })

  await page.goto("/chat/env-a")

  // 连续工具调用合并为一个分组行，工具标题收起不可见。
  await expect(page.getByText("Used 3 tools")).toBeVisible()
  await expect(page.getByText("18s")).toBeVisible()
  await expect(page.getByText("Run command")).toHaveCount(0)

  // 展开分组后逐个工具可见，再展开单个工具显示命令与标准输出。
  await page.getByRole("button", { name: "Expand Used 3 tools" }).click()
  await expect(page.getByText("Run command")).toBeVisible()
  await expect(page.getByText("Search files")).toBeVisible()
  await page.getByRole("button", { name: "Expand Run command" }).click()
  await expect(page.getByText("Command", { exact: true })).toBeVisible()
  await expect(page.getByText("Standard output")).toBeVisible()
  await expect(page.getByText("test passed")).toBeVisible()

  // 完成的思考收起为一行时长摘要，点击展开显示内容。
  await expect(page.getByText("Thought for 9s")).toBeVisible()
  await page.getByRole("button", { name: "Expand reasoning" }).click()
  await expect(page.getByText("需要先检查配置")).toBeVisible()

  // 文字输出之后的单个工具渲染为紧凑行并带路径摘要。
  await expect(page.getByText("Apply patch")).toBeVisible()
  await expect(page.getByText("docs/web-workbench.md")).toBeVisible()
})

async function installApiFixture(page: Page) {
  await page.route("**/*", async (route) => {
    const request = route.request()
    if (request.isNavigationRequest()) return route.continue()
    const path = new URL(request.url()).pathname
    if (path === "/environments") return json(route, [environment("env-a", "Environment A", "thread-a"), environment("env-b", "Environment B", "thread-b")])
    if (path === "/environments/env-a") return json(route, environment("env-a", "Environment A", "thread-a"))
    if (path === "/environments/env-b") return json(route, environment("env-b", "Environment B", "thread-b"))
    if (path === "/skills") return json(route, { skills: [], roots: [], errors: [] })
    if (path.startsWith("/threads/") && path.endsWith("/messages") && request.method() === "POST") return json(route, { turn_id: "turn-next" })
    return route.continue()
  })
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
    state: { resource: "ready", runtime: { lifecycle: "active", activity: "idle", active_turn: null, pending_inputs: 0 } },
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

/** 覆盖分组场景：思考、三连工具调用、文字输出、单个工具调用。 */
function groupedSnapshot() {
  const threadId = "thread-a"
  const turnId = "turn-grouped"
  let ordinal = 0
  const item = (content: unknown, status: string, createdAt: number, completedAt: number) => {
    ordinal += 1
    return { id: `${turnId}:item-${ordinal}`, threadId, turnId, ordinal, revision: 0, status, createdAt, updatedAt: completedAt, completedAt, content }
  }
  const tool = (name: string, argumentsJson: string | undefined, resultJson: string | undefined, createdAt: number, completedAt: number, extra: Record<string, unknown> = {}) =>
    item({ type: "toolCall", tool: { toolCallId: `call-${name}`, name, arguments: argumentsJson, result: resultJson, timedOut: false, ...extra } }, "completed", createdAt, completedAt)

  return {
    type: "snapshot",
    snapshot: {
      schemaVersion: 1,
      revision: 1,
      thread: thread(threadId, threadId),
      items: [
        item({ type: "userMessage", text: "请检查 PR" }, "completed", 0, 1_000),
        item({ type: "reasoning", summary: ["先看测试"], content: ["需要先检查配置"] }, "completed", 0, 9_000),
        tool("exec", JSON.stringify({ command: "cargo test -p mai-server" }), JSON.stringify({ status: "exit", exitCode: 0, stdout: "test passed", stderr: "" }), 10_000, 16_000),
        tool("read_file", JSON.stringify({ path: "README.md" }), undefined, 16_000, 22_000),
        tool("search_files", JSON.stringify({ query: "timeline", path: "crates" }), undefined, 22_000, 28_000),
        item({ type: "agentMessage", channel: "final", text: "工具运行完成，以下是结论。" }, "completed", 28_000, 29_000),
        tool("apply_patch", JSON.stringify({ input: "*** Update File: docs/web-workbench.md" }), undefined, 29_000, 30_000),
      ],
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
