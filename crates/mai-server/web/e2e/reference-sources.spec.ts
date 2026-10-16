import { expect, test, type Page, type Route } from "@playwright/test"

/**
 * 参考源码管理面与项目面的端到端冒烟。
 *
 * 全部数据由本文件 mock（沿用仓库现有 e2e 风格）：不依赖真实 Docker、宿主目录或参考源码
 * 服务端。desktop/tablet/mobile 三个视口都会执行每个用例，并在关键断言点保存截图入口。
 *
 * 契约要点（与 mai-server handlers/references 一致）：
 * - `GET/PUT /projects/{id}/references` 只返回 `{revision,bindings}`。
 * - `GET /projects/{id}/references/status` 与 `POST .../apply` 返回完整 `ProjectReferenceStatus`。
 * - `GET /reference-sources` 是裸数组。
 */

interface Row {
  id: string
  alias: string
}

interface ProjectState {
  revision: number
  rows: Row[]
}

interface SaveGate {
  promise: Promise<void>
  release(): void
}

interface Fixture {
  /** 保存请求的延迟闸门；为 null 时立即返回。 */
  saveGate: SaveGate | null
  /** 下一次 PUT 返回 409 并把服务端推进到 revision 3。 */
  conflictOnSave: boolean
  /** 记录次数，用于证明 SSE 只触发重新取数。 */
  configRequests: number
  statusRequests: number
}

const PROJECT_ID = "project-1"
const OTHER_PROJECT_ID = "project-2"

let fixture: Fixture
let projects: Record<string, ProjectState>

test.beforeEach(async ({ page }) => {
  fixture = { saveGate: null, conflictOnSave: false, configRequests: 0, statusRequests: 0 }
  projects = {
    [PROJECT_ID]: { revision: 0, rows: [{ id: "binding-1", alias: "linux" }] },
    [OTHER_PROJECT_ID]: { revision: 0, rows: [] },
  }
  await page.addInitScript(() => {
    const instances: EventTarget[] = []
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
        instances.push(this)
        window.setTimeout(() => this.onopen?.(new Event("open")), 0)
      }
      close() { this.readyState = QuietEventSource.CLOSED }
    }
    Object.defineProperty(window, "EventSource", { configurable: true, value: QuietEventSource })
    const emitter = window as unknown as {
      __emitProductEvent?: (type: string, payload: Record<string, unknown>) => void
    }
    emitter.__emitProductEvent = (type, payload) => {
      const data = JSON.stringify({ sequence: 1, timestamp: "2026-10-15T00:00:00Z", type, ...payload })
      for (const instance of instances) instance.dispatchEvent(new MessageEvent(type, { data }))
    }
  })
  await installReferenceFixture(page)
})

test("项目 References 标签编辑并保存参考源绑定", async ({ page }) => {
  await page.goto(`/projects/${PROJECT_ID}?view=references`)

  await expect(page.getByRole("heading", { name: "Reference sources" })).toBeVisible()
  await expect(page.getByLabel("Alias")).toHaveValue("linux")
  await expect(page.getByText("/references/linux")).toBeVisible()
  await capture(page, "project-references")

  await page.getByLabel("Alias").fill("kernel")
  await expect(page.getByTestId("reference-draft-dirty")).toBeVisible()

  const saved = page.waitForRequest((request) => request.url().endsWith(`/projects/${PROJECT_ID}/references`) && request.method() === "PUT")
  await page.getByRole("button", { name: "Save and apply" }).click()
  expect((await saved).postDataJSON()).toMatchObject({ expected_revision: 0, bindings: [{ id: "binding-1", alias: "kernel" }] })

  await expect(page.getByTestId("reference-draft-dirty")).toHaveCount(0)
  await expect(page.getByRole("button", { name: "Save and apply" })).toBeDisabled()
  await capture(page, "project-references-saved")

  // 配置已保存 != 全部已应用：等待中的应用事实来自 status。
  await expect(page.getByTestId("reference-application-summary")).toContainText("waiting")
  await page.getByRole("button", { name: "Stop tasks and apply now" }).click()
  await expect(page.getByRole("alertdialog")).toContainText("Stop running tasks")
  await capture(page, "project-references-stop-and-apply")
  await page.getByRole("button", { name: "Confirm stop and apply" }).click()
  await expect(page.getByRole("alertdialog")).toHaveCount(0)
})

test("保存期间继续编辑保留草稿，SSE 只重新取数", async ({ page }) => {
  fixture.saveGate = deferred()
  await page.goto(`/projects/${PROJECT_ID}?view=references`)

  await page.getByLabel("Alias").fill("kernel")
  await page.getByRole("button", { name: "Save and apply" }).click()

  // 请求在飞行中：编辑不被禁用，新输入不被服务器返回值覆盖。
  await expect(page.getByLabel("Alias")).toBeEnabled()
  await page.getByLabel("Alias").fill("kernel2")
  await expect(page.getByTestId("reference-draft-dirty")).toBeVisible()
  await capture(page, "project-references-editing-during-save")

  const before = fixture.configRequests
  await emitProductEvent(page, "project_references_changed", { project_id: PROJECT_ID, revision: 0 })
  await expect.poll(() => fixture.configRequests).toBeGreaterThan(before)
  await expect(page.getByLabel("Alias")).toHaveValue("kernel2")
  await expect(page.getByTestId("reference-draft-dirty")).toBeVisible()

  fixture.saveGate?.release()
  // 保存收敛后基线前进到 revision 1，但飞行中打的 kernel2 仍是未保存草稿。
  await expect(page.getByRole("button", { name: "Save and apply" })).toBeEnabled()
  // 乱序事实以更新的为准：SSE 触发的旧 revision 0 取数不能覆盖保存后的 revision 1。
  await expect(page.getByTestId("reference-configuration-summary")).toContainText("Saved configuration revision 1.")
  await expect(page.getByLabel("Alias")).toHaveValue("kernel2")
  await expect(page.getByTestId("reference-draft-dirty")).toBeVisible()
})

test("切换项目时保留各自草稿，不把 A 的编辑套用到 B", async ({ page }) => {
  page.on("dialog", (dialog) => void dialog.accept())
  await page.goto(`/projects/${PROJECT_ID}?view=references`)

  await page.getByLabel("Alias").fill("kernel")
  await selectProject(page, "owner/other")

  await expect(page).toHaveURL(new RegExp(`/projects/${OTHER_PROJECT_ID}`))
  await expect(page.getByText("No reference sources bound")).toBeVisible()

  await selectProject(page, "owner/repo")
  await expect(page.getByLabel("Alias")).toHaveValue("kernel")
  await expect(page.getByTestId("reference-draft-dirty")).toBeVisible()
  await capture(page, "project-references-draft-per-project")
})

test("CAS 冲突显示服务端版本且不覆盖本地草稿", async ({ page }) => {
  fixture.conflictOnSave = true
  await page.goto(`/projects/${PROJECT_ID}?view=references`)

  await page.getByLabel("Alias").fill("kernel")
  await page.getByRole("button", { name: "Save and apply" }).click()

  await expect(page.getByText("Configuration revision conflict")).toBeVisible()
  await expect(page.getByText(/server now has revision 3/)).toBeVisible()
  await expect(page.getByLabel("Alias")).toHaveValue("kernel")
  await capture(page, "project-references-cas-conflict")

  await page.getByRole("button", { name: "Reload server revision 3" }).click()
  await expect(page.getByLabel("Alias")).toHaveValue("from-server")
  await expect(page.getByTestId("reference-draft-dirty")).toHaveCount(0)
})

test("设置页登记参考源并显示验证进度", async ({ page }) => {
  await page.goto("/settings/reference-sources")

  await expect(page.getByRole("button", { name: "Register source" })).toBeVisible()
  await expect(page.getByRole("cell", { name: /^Linux reference \/srv\/mai-/ })).toContainText("/srv/mai-references/source-1/repo")
  await expect(page.getByText("scanning")).toBeVisible()
  await capture(page, "settings-reference-sources")

  await page.getByRole("button", { name: "Register source" }).click()
  await expect(page.getByRole("dialog")).toBeVisible()
  await page.getByLabel("Name").fill("QEMU reference")
  await page.getByLabel("Host path (server)").fill("/srv/mai-references/qemu/repo")
  await page.getByLabel("Version label").fill("Published QEMU snapshot")
  await capture(page, "settings-reference-sources-register")
  await page.getByRole("switch", { name: /suitable to expose to agents and models/ }).click()
  await page.getByRole("button", { name: "Register and verify" }).click()
  await expect(page.getByRole("dialog")).toHaveCount(0)
})

function deferred(): SaveGate {
  let release: () => void = () => undefined
  const promise = new Promise<void>((resolve) => {
    release = resolve
  })
  return { promise, release }
}

/** SPA 内切换项目：移动端先打开资源抽屉。未保存草稿会先经过确认弹窗。 */
async function selectProject(page: Page, name: string) {
  if ((page.viewportSize()?.width ?? 0) < 1024) {
    await page.getByRole("button", { name: "Open Projects" }).click()
  }
  await page.getByRole("button", { name: new RegExp(name) }).click()
  const id = name === "owner/other" ? OTHER_PROJECT_ID : PROJECT_ID
  await expect(page).toHaveURL(new RegExp(`/projects/${id}`))
  await page.getByRole("tab", { name: "References", exact: true }).click()
}

async function emitProductEvent(page: Page, type: string, payload: Record<string, unknown>) {
  await page.evaluate(([eventType, eventPayload]) => {
    const emitter = window as unknown as {
      __emitProductEvent?: (type: string, payload: Record<string, unknown>) => void
    }
    emitter.__emitProductEvent?.(eventType, eventPayload)
  }, [type, payload] as const)
}

async function capture(page: Page, name: string) {
  const width = page.viewportSize()?.width ?? 0
  await page.screenshot({ path: test.info().outputPath(`${name}-${width}.png`), fullPage: true })
}

function bindings(state: ProjectState) {
  return state.rows.map((row) => ({
    id: row.id,
    source_id: "source-1",
    alias: row.alias,
    description: "参考 IRQ 与驱动生命周期",
    enabled: true,
  }))
}

function referenceConfiguration(projectId: string) {
  const state = projects[projectId]
  return { revision: state.revision, bindings: bindings(state) }
}

function referenceStatus(projectId: string) {
  const state = projects[projectId]
  return {
    project_id: projectId,
    configuration: { revision: state.revision, bindings: bindings(state) },
    target_revision: state.revision,
    state: state.revision === 0 ? "applied" : "pending",
    counts: state.revision === 0
      ? { applied: 1, waiting: 0, failed: 0, deferred: 0, no_environment: 0 }
      : { applied: 0, waiting: 1, failed: 0, deferred: 0, no_environment: 0 },
    agents: [{
      agent_id: "agent-1",
      project_id: projectId,
      applied_revision: 0,
      state: state.revision === 0 ? "applied" : "waiting_idle",
      observed_at: "2026-10-15T00:00:00Z",
    }],
    page: { offset: 0, limit: 20, total: 1 },
    observed_at: "2026-10-15T00:00:00Z",
  }
}

async function installReferenceFixture(page: Page) {
  await page.route("**/*", async (route) => {
    const request = route.request()
    if (request.isNavigationRequest()) return route.continue()
    const url = new URL(request.url())
    const path = url.pathname
    const method = request.method()

    if (path === "/projects") return json(route, [projectSummary(PROJECT_ID, "owner/repo"), projectSummary(OTHER_PROJECT_ID, "owner/other")])
    const detailMatch = /^\/projects\/([^/]+)$/.exec(path)
    if (detailMatch && method === "GET") return json(route, projectDetail(detailMatch[1]))

    if (path === "/reference-sources" && method === "POST") {
      return route.fulfill({ status: 202, contentType: "application/json", body: JSON.stringify(validatingSource) })
    }
    if (path === "/reference-sources") return json(route, [readySource, validatingSource])
    if (path === "/reference-source-policy") return json(route, policy)

    const projectReferences = /^\/projects\/([^/]+)\/references$/.exec(path)
    if (projectReferences) {
      const projectId = projectReferences[1]
      if (method === "PUT") {
        if (fixture.saveGate) await fixture.saveGate.promise
        if (fixture.conflictOnSave) {
          fixture.conflictOnSave = false
          projects[projectId] = { revision: 3, rows: [{ id: "binding-9", alias: "from-server" }] }
          return json(route, {
            code: "reference_revision_conflict",
            message: "配置版本已变化，请刷新后重试",
            field_errors: [{ field: "expected_revision", message: "期望版本与当前版本不一致" }],
            request_id: "req-e2e",
            retryable: false,
          }, 409)
        }
        const body = request.postDataJSON() as { expected_revision: number; bindings: Row[] }
        projects[projectId] = {
          revision: body.expected_revision + 1,
          rows: body.bindings.map((row) => ({ id: row.id, alias: row.alias })),
        }
        return json(route, referenceConfiguration(projectId))
      }
      fixture.configRequests += 1
      return json(route, referenceConfiguration(projectId))
    }

    const projectStatus = /^\/projects\/([^/]+)\/references\/status$/.exec(path)
    if (projectStatus) {
      fixture.statusRequests += 1
      return json(route, referenceStatus(projectStatus[1]))
    }
    const projectApply = /^\/projects\/([^/]+)\/references\/apply$/.exec(path)
    if (projectApply) {
      return route.fulfill({ status: 202, contentType: "application/json", body: JSON.stringify(referenceStatus(projectApply[1])) })
    }
    return route.continue()
  })
}

const readySource = {
  id: "source-1",
  name: "Linux reference",
  revision_label: "v6.12",
  host_path: "/srv/mai-references/source-1/repo",
  provenance: { kind: "git_tree", commit_sha: "f".repeat(40), tree_sha: "a".repeat(40) },
  manifest_digest: "sha256:tree",
  state: "ready",
  approved_for_agent_access: true,
  file_count: 120,
  byte_count: 4096,
  source_device: 2049,
  source_inode: 42,
  warnings: [],
  created_at: "2026-10-15T00:00:00Z",
  updated_at: "2026-10-15T00:00:00Z",
}

const validatingSource = {
  ...readySource,
  id: "source-2",
  name: "QEMU reference",
  host_path: "/srv/mai-references/source-2/repo",
  state: "validating",
  manifest_digest: null,
  file_count: 0,
  byte_count: 0,
}

const policy = {
  allowed_roots: ["/srv/mai-references"],
  trusted_owner_uids: [0],
  reference_container_root: "/references",
  alias_pattern: "^[a-z][a-z0-9_-]{0,31}$",
  max_alias_chars: 32,
  max_bindings_per_project: 16,
  max_description_chars: 512,
  source_name_max_chars: 128,
  revision_label_max_chars: 256,
  manifest_file_name: "source-manifest.json",
  supported_provenance_kinds: ["git_tree", "directory_snapshot"],
  require_strict_recursive_read_only: true,
  minimum_docker_engine_version: "25.0",
  minimum_kernel_version: "5.12",
}

function projectSummary(id: string, name: string) {
  return {
    id,
    name,
    repository: name,
    repository_full_name: name,
    status: "ready",
    maintainer_agent_id: "agent-1",
  }
}

function projectDetail(id: string) {
  const summary = id === OTHER_PROJECT_ID ? projectSummary(OTHER_PROJECT_ID, "owner/other") : projectSummary(PROJECT_ID, "owner/repo")
  const agent = {
    id: "agent-1",
    name: "Maintainer",
    role: "planner",
    resource: { state: "ready", error: null },
    provider_id: "fixture",
    provider_name: "Fixture",
    model: "fixture-model",
    created_at: "2026-10-15T00:00:00Z",
    updated_at: "2026-10-15T00:00:00Z",
    thread: { id: "agent-1", projectId: id, title: "Maintainer", mode: "simple", rootThreadId: "agent-1", role: "planner", agentPath: "root", status: "idle", createdAt: 1, updatedAt: 1, archived: false },
  }
  return { ...summary, agents: [agent], auth_status: "ready", mcp_status: "ready" }
}

function json(route: Route, body: unknown, status = 200) {
  return route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) })
}
