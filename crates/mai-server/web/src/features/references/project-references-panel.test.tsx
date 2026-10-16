import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { render, screen, waitFor } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { MemoryRouter } from "react-router-dom"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

import type {
  ProjectReferenceBinding,
  ProjectReferenceConfiguration,
  ProjectReferenceStatus,
  ReferenceSource,
} from "@/api/reference-types"
import { applyProductEvent } from "@/events/product-event-controller"
import { ProjectReferencesPanel } from "@/features/references/project-references-panel"
import { referenceDraftStore } from "@/features/references/reference-draft-store"

const PROJECT_ID = "project-1"
const OTHER_PROJECT_ID = "project-2"
const OBSERVED_AT = "2026-10-15T00:00:00Z"

function readySource(id: string, name: string, inode: number): ReferenceSource {
  return {
    id,
    name,
    revision_label: "v6.12",
    host_path: `/srv/mai-references/${id}/repo`,
    provenance: { kind: "git_tree", commit_sha: "f".repeat(40), tree_sha: "a".repeat(40) },
    manifest_digest: "sha256:tree",
    state: "ready",
    approved_for_agent_access: true,
    file_count: 120,
    byte_count: 4096,
    source_device: 2049,
    source_inode: inode,
    warnings: [],
    created_at: OBSERVED_AT,
    updated_at: OBSERVED_AT,
  }
}

const linuxSource = readySource("source-1", "Linux reference", 42)
const qemuSource = readySource("source-2", "QEMU reference", 43)

function binding(
  id: string,
  sourceId: string,
  alias: string,
  overrides: Partial<ProjectReferenceBinding> = {},
): ProjectReferenceBinding {
  return { id, source_id: sourceId, alias, description: "参考内核", enabled: true, ...overrides }
}

/** `GET/PUT /projects/{id}/references` 的唯一响应形状。 */
function configuration(revision: number, bindings: ProjectReferenceBinding[]): ProjectReferenceConfiguration {
  return { revision, bindings }
}

/** `GET .../references/status` 与 `POST .../references/apply` 的响应形状。 */
function projectStatus(
  revision: number,
  bindings: ProjectReferenceBinding[],
  overrides: Partial<ProjectReferenceStatus> = {},
): ProjectReferenceStatus {
  return {
    project_id: PROJECT_ID,
    configuration: configuration(revision, bindings),
    target_revision: revision,
    state: "applied",
    counts: { applied: 1, waiting: 0, failed: 0, deferred: 0, no_environment: 0 },
    agents: [{
      agent_id: "agent-1",
      project_id: PROJECT_ID,
      applied_revision: revision,
      state: "applied",
      observed_at: OBSERVED_AT,
    }],
    page: { offset: 0, limit: 20, total: 1 },
    observed_at: OBSERVED_AT,
    ...overrides,
  }
}

interface Call {
  path: string
  method: string
  body: unknown
  search: string
}

type HandlerResult = { status?: number; body?: unknown } | undefined
type Handler = (call: Call) => HandlerResult | Promise<HandlerResult>

function installFetch(handle: Handler) {
  const calls: Call[] = []
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input), "http://127.0.0.1")
    const call: Call = {
      path: url.pathname,
      search: url.search,
      method: (init?.method ?? "GET").toUpperCase(),
      body: init?.body ? JSON.parse(String(init.body)) : null,
    }
    calls.push(call)
    const result = await handle(call)
    if (!result) return new Response("not found", { status: 404 })
    return new Response(result.body === undefined ? "" : JSON.stringify(result.body), {
      status: result.status ?? 200,
      headers: { "content-type": "application/json" },
    })
  }))
  return calls
}

function renderPanel(projectId: string = PROJECT_ID) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  const view = render(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <ProjectReferencesPanel projectId={projectId} />
      </MemoryRouter>
    </QueryClientProvider>,
  )
  return { client, unmount: view.unmount }
}

/** 单项目、单绑定、读服务器配置与状态的最小 fixture。 */
function singleBindingFixture() {
  return installFetch((call) => {
    if (call.path === "/reference-sources") return { body: [linuxSource] }
    if (call.path === `/projects/${PROJECT_ID}/references/status`) {
      return { body: projectStatus(0, [binding("b1", linuxSource.id, "linux")]) }
    }
    if (call.path === `/projects/${PROJECT_ID}/references` && call.method === "PUT") {
      return { body: configuration(1, [binding("b1", linuxSource.id, "kernel")]) }
    }
    if (call.path === `/projects/${PROJECT_ID}/references`) {
      return { body: configuration(0, [binding("b1", linuxSource.id, "linux")]) }
    }
    return undefined
  })
}

beforeEach(() => {
  referenceDraftStore.resetAll()
})

afterEach(() => {
  vi.unstubAllGlobals()
  referenceDraftStore.resetAll()
})

describe("project references panel", () => {
  it("reads the durable configuration and the application status from separate endpoints", async () => {
    const calls = singleBindingFixture()
    renderPanel()

    expect(await screen.findByLabelText("Alias")).toHaveValue("linux")
    expect(calls.some((call) => call.path === `/projects/${PROJECT_ID}/references` && call.method === "GET")).toBe(true)
    expect(calls.some((call) => call.path === `/projects/${PROJECT_ID}/references/status`)).toBe(true)
  })

  it("saves the captured base revision and keeps the draft afterwards", async () => {
    let revision = 0
    const calls = installFetch((call) => {
      if (call.path === "/reference-sources") return { body: [linuxSource] }
      const current = () => [binding("b1", linuxSource.id, "linux", { alias: revision === 0 ? "linux" : "kernel", description: "参考内核" })]
      if (call.path === `/projects/${PROJECT_ID}/references/status`) return { body: projectStatus(revision, current()) }
      if (call.path === `/projects/${PROJECT_ID}/references` && call.method === "PUT") {
        revision = 1
        return { body: configuration(revision, [binding("b1", linuxSource.id, "kernel")]) }
      }
      if (call.path === `/projects/${PROJECT_ID}/references`) return { body: configuration(revision, current()) }
      return undefined
    })
    renderPanel()

    const alias = await screen.findByLabelText("Alias")
    await userEvent.clear(alias)
    await userEvent.type(alias, "kernel")
    expect(await screen.findByTestId("reference-draft-dirty")).toBeInTheDocument()

    await userEvent.click(screen.getByRole("button", { name: /Save and apply/ }))

    await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true))
    expect(calls.find((call) => call.method === "PUT")?.body).toEqual({
      expected_revision: 0,
      bindings: [{ id: "b1", source_id: "source-1", alias: "kernel", description: "参考内核", enabled: true }],
    })
    await waitFor(() => expect(screen.queryByTestId("reference-draft-dirty")).not.toBeInTheDocument())
    expect(screen.getByLabelText("Alias")).toHaveValue("kernel")
  })

  it("submits at most one request for a double click", async () => {
    const calls = singleBindingFixture()
    renderPanel()

    const alias = await screen.findByLabelText("Alias")
    await userEvent.clear(alias)
    await userEvent.type(alias, "kernel")
    const save = screen.getByRole("button", { name: /Save and apply/ })
    await userEvent.click(save)
    await userEvent.click(save)

    await waitFor(() => expect(calls.filter((call) => call.method === "PUT")).toHaveLength(1))
  })

  it("refuses to save a duplicate source binding the server would reject", async () => {
    singleBindingFixture()
    renderPanel()

    await screen.findByLabelText("Alias")
    await userEvent.click(screen.getByRole("button", { name: /Add binding/ }))

    // 新增行默认选中同一个 ready 源：客户端必须先判重，而不是等 409/422。
    expect(await screen.findAllByText("同一参考源在同一项目内只能绑定一次。")).toHaveLength(2)
    expect(screen.getByRole("button", { name: /Save and apply/ })).toBeDisabled()
  })

  it.each([
    { removed: "first", remaining: ["second", "third"] },
    { removed: "second", remaining: ["first", "third"] },
  ])("removes $removed by stable id and preserves the remaining DOM identity", async ({ removed, remaining }) => {
    installFetch((call) => {
      if (call.path === "/reference-sources") return { body: [linuxSource, qemuSource] }
      const rows = [
        binding("b1", linuxSource.id, "first"),
        binding("b2", qemuSource.id, "second"),
        binding("b3", linuxSource.id, "third", { source_id: "source-3" }),
      ]
      if (call.path === `/projects/${PROJECT_ID}/references/status`) return { body: projectStatus(0, rows) }
      if (call.path === `/projects/${PROJECT_ID}/references`) return { body: configuration(0, rows) }
      return undefined
    })
    renderPanel()

    expect(await screen.findAllByLabelText("Alias")).toHaveLength(3)
    const third = screen.getByDisplayValue("third")
    await userEvent.click(screen.getByRole("button", { name: `Remove binding ${removed}` }))

    const aliases = screen.getAllByLabelText("Alias").map((input) => (input as HTMLInputElement).value)
    expect(aliases).toEqual(remaining)
    expect(screen.getByDisplayValue("third")).toBe(third)
    await userEvent.click(third)
    expect(third).toHaveFocus()
    await userEvent.type(third, "-edited")
    expect(screen.getByDisplayValue("third-edited")).toBe(third)
  })

  it("merges edits typed while the save request is in flight", async () => {
    let releasePut: () => void = () => { throw new Error("Save promise is not initialized") }
    const gate = new Promise<void>((resolve) => {
      releasePut = resolve
    })
    installFetch(async (call) => {
      if (call.path === "/reference-sources") return { body: [linuxSource] }
      if (call.path === `/projects/${PROJECT_ID}/references/status`) return { body: projectStatus(0, [binding("b1", linuxSource.id, "linux")]) }
      if (call.path === `/projects/${PROJECT_ID}/references` && call.method === "PUT") {
        await gate
        return { body: configuration(1, [binding("b1", linuxSource.id, "kernel")]) }
      }
      if (call.path === `/projects/${PROJECT_ID}/references`) return { body: configuration(0, [binding("b1", linuxSource.id, "linux")]) }
      return undefined
    })
    renderPanel()

    const alias = await screen.findByLabelText("Alias")
    await userEvent.clear(alias)
    await userEvent.type(alias, "kernel")
    await userEvent.click(screen.getByRole("button", { name: /Save and apply/ }))

    // 保存进行中继续编辑：输入不被禁用，且新值不会被服务器返回值覆盖。
    const editing = screen.getByLabelText("Alias")
    expect(editing).toBeEnabled()
    await userEvent.type(editing, "2")
    expect(screen.getByLabelText("Alias")).toHaveValue("kernel2")

    releasePut()
    await waitFor(() => expect(screen.getByRole("button", { name: /Save and apply/ })).toBeEnabled())
    expect(screen.getByLabelText("Alias")).toHaveValue("kernel2")
    // 基线已经前进到服务器返回的 revision 1，草稿仍是未保存的 kernel2。
    expect(screen.getByTestId("reference-draft-dirty")).toBeInTheDocument()
  })

  it("keeps the draft and its in-flight edits when the panel is reopened for the same project", async () => {
    let releasePut: () => void = () => { throw new Error("Save promise is not initialized") }
    const gate = new Promise<void>((resolve) => {
      releasePut = resolve
    })
    installFetch(async (call) => {
      if (call.path === "/reference-sources") return { body: [linuxSource] }
      if (call.path === `/projects/${PROJECT_ID}/references/status`) return { body: projectStatus(0, [binding("b1", linuxSource.id, "linux")]) }
      if (call.path === `/projects/${PROJECT_ID}/references` && call.method === "PUT") {
        await gate
        return { body: configuration(1, [binding("b1", linuxSource.id, "kernel")]) }
      }
      if (call.path === `/projects/${PROJECT_ID}/references`) return { body: configuration(0, [binding("b1", linuxSource.id, "linux")]) }
      return undefined
    })
    const first = renderPanel()

    const alias = await screen.findByLabelText("Alias")
    await userEvent.clear(alias)
    await userEvent.type(alias, "kernel")
    await userEvent.click(screen.getByRole("button", { name: /Save and apply/ }))
    await userEvent.type(screen.getByLabelText("Alias"), "2")

    // 卸载（例如切到别的标签页/路由）：草稿与未完成的保存都必须留下。
    first.unmount()

    renderPanel()
    expect(await screen.findByLabelText("Alias")).toHaveValue("kernel2")
    expect(screen.getByTestId("reference-draft-dirty")).toBeInTheDocument()

    releasePut()
    await waitFor(() => expect(referenceDraftStore.peek(PROJECT_ID)?.saving).toBe(false))
    expect(screen.getByLabelText("Alias")).toHaveValue("kernel2")
    expect(screen.getByTestId("reference-draft-dirty")).toBeInTheDocument()
  })

  it("keeps one draft per project when switching projects", async () => {
    installFetch((call) => {
      if (call.path === "/reference-sources") return { body: [linuxSource, qemuSource] }
      if (call.path === `/projects/${PROJECT_ID}/references/status`) return { body: projectStatus(0, [binding("b1", linuxSource.id, "linux")]) }
      if (call.path === `/projects/${PROJECT_ID}/references`) return { body: configuration(0, [binding("b1", linuxSource.id, "linux")]) }
      if (call.path === `/projects/${OTHER_PROJECT_ID}/references/status`) return { body: projectStatus(0, []) }
      if (call.path === `/projects/${OTHER_PROJECT_ID}/references`) return { body: configuration(0, []) }
      return undefined
    })
    const first = renderPanel(PROJECT_ID)

    const alias = await screen.findByLabelText("Alias")
    await userEvent.clear(alias)
    await userEvent.type(alias, "kernel")
    first.unmount()

    // 另一个项目的草稿为空：绝不套用 project-1 的编辑。
    const second = renderPanel(OTHER_PROJECT_ID)
    expect(await screen.findByText("No reference sources bound")).toBeInTheDocument()
    second.unmount()

    // 回到第一个项目，草稿仍在。
    renderPanel(PROJECT_ID)
    expect(await screen.findByLabelText("Alias")).toHaveValue("kernel")
  })

  it("shows the server revision on a CAS conflict and never overwrites the local draft", async () => {
    let revision = 0
    const calls = installFetch((call) => {
      if (call.path === "/reference-sources") return { body: [linuxSource] }
      const rows = () => revision === 0
        ? [binding("b1", linuxSource.id, "linux")]
        : [binding("b9", linuxSource.id, "from-server")]
      if (call.path === `/projects/${PROJECT_ID}/references/status`) return { body: projectStatus(revision, rows()) }
      if (call.path === `/projects/${PROJECT_ID}/references` && call.method === "PUT") {
        revision = 3
        return {
          status: 409,
          body: {
            code: "reference_revision_conflict",
            message: "配置版本已变化，请刷新后重试",
            field_errors: [{ field: "expected_revision", message: "期望版本与当前版本不一致" }],
            request_id: "req-1",
            retryable: false,
          },
        }
      }
      if (call.path === `/projects/${PROJECT_ID}/references`) return { body: configuration(revision, rows()) }
      return undefined
    })
    renderPanel()

    const alias = await screen.findByLabelText("Alias")
    await userEvent.clear(alias)
    await userEvent.type(alias, "kernel")
    await userEvent.click(screen.getByRole("button", { name: /Save and apply/ }))

    expect(await screen.findByText(/Configuration revision conflict/)).toBeInTheDocument()
    await waitFor(() => expect(screen.getByText(/server now has revision 3/)).toBeInTheDocument())
    expect(screen.getByLabelText("Alias")).toHaveValue("kernel")

    await userEvent.click(screen.getByRole("button", { name: "Reload server revision 3" }))
    await waitFor(() => expect(screen.getByLabelText("Alias")).toHaveValue("from-server"))
    expect(calls.filter((call) => call.method === "PUT")).toHaveLength(1)
  })

  it("keeps the draft when the save request fails at the network layer", async () => {
    installFetch((call) => {
      if (call.path === "/reference-sources") return { body: [linuxSource] }
      if (call.path === `/projects/${PROJECT_ID}/references/status`) return { body: projectStatus(0, [binding("b1", linuxSource.id, "linux")]) }
      if (call.path === `/projects/${PROJECT_ID}/references` && call.method === "PUT") {
        throw new Error("network down")
      }
      if (call.path === `/projects/${PROJECT_ID}/references`) return { body: configuration(0, [binding("b1", linuxSource.id, "linux")]) }
      return undefined
    })
    renderPanel()

    const alias = await screen.findByLabelText("Alias")
    await userEvent.clear(alias)
    await userEvent.type(alias, "kernel")
    await userEvent.click(screen.getByRole("button", { name: /Save and apply/ }))

    expect(await screen.findByText("network down")).toBeInTheDocument()
    expect(screen.getByLabelText("Alias")).toHaveValue("kernel")
    expect(screen.getByTestId("reference-draft-dirty")).toBeInTheDocument()
  })

  it("does not rebuild the draft when a product event invalidates the server facts", async () => {
    const calls = singleBindingFixture()
    const { client } = renderPanel()

    const alias = await screen.findByLabelText("Alias")
    await userEvent.clear(alias)
    await userEvent.type(alias, "kernel")

    applyProductEvent(client, { sequence: 2, timestamp: OBSERVED_AT, type: "project_references_changed", project_id: PROJECT_ID, revision: 1 })
    applyProductEvent(client, { sequence: 3, timestamp: OBSERVED_AT, type: "agent_reference_application_changed", project_id: PROJECT_ID, agent_id: "agent-1" })
    applyProductEvent(client, { sequence: 4, timestamp: OBSERVED_AT, type: "reference_source_changed", source_id: linuxSource.id })

    // 事件只让服务器事实重新取数：配置与状态被重新请求，草稿保持用户输入。
    await waitFor(() => expect(calls.filter((call) => call.path === `/projects/${PROJECT_ID}/references`).length).toBeGreaterThan(1))
    expect(screen.getByLabelText("Alias")).toHaveValue("kernel")
    expect(screen.getByTestId("reference-draft-dirty")).toBeInTheDocument()
  })

  it("requires the confirmation dialog before stopping tasks to apply", async () => {
    const calls = installFetch((call) => {
      if (call.path === "/reference-sources") return { body: [linuxSource] }
      if (call.path === `/projects/${PROJECT_ID}/references/status`) {
        return { body: projectStatus(1, [binding("b1", linuxSource.id, "linux")], { state: "pending", counts: { applied: 0, waiting: 1, failed: 0, deferred: 0, no_environment: 0 } }) }
      }
      if (call.path === `/projects/${PROJECT_ID}/references`) return { body: configuration(1, [binding("b1", linuxSource.id, "linux")]) }
      if (call.path === `/projects/${PROJECT_ID}/references/apply`) {
        return { status: 202, body: projectStatus(1, [binding("b1", linuxSource.id, "linux")], { state: "pending" }) }
      }
      return undefined
    })
    renderPanel()

    await userEvent.click(await screen.findByRole("button", { name: /Stop tasks and apply now/ }))
    expect(await screen.findByText("Stop running tasks and apply references?")).toBeInTheDocument()
    expect(calls.some((call) => call.path.endsWith("/apply"))).toBe(false)

    await userEvent.click(screen.getByRole("button", { name: "Confirm stop and apply" }))
    await waitFor(() => expect(calls.some((call) => call.path.endsWith("/apply"))).toBe(true))
    expect(calls.find((call) => call.path.endsWith("/apply"))?.body).toEqual({ expected_revision: 1, policy: "stop_and_apply" })
  })

  it("pages the agent status list with the full counts intact", async () => {
    const calls = installFetch((call) => {
      if (call.path === "/reference-sources") return { body: [linuxSource] }
      if (call.path === `/projects/${PROJECT_ID}/references`) return { body: configuration(1, [binding("b1", linuxSource.id, "linux")]) }
      if (call.path === `/projects/${PROJECT_ID}/references/status`) {
        const offset = Number(new URLSearchParams(call.search).get("offset") || "0")
        return {
          body: projectStatus(1, [binding("b1", linuxSource.id, "linux")], {
            state: "pending",
            counts: { applied: 0, waiting: 25, failed: 0, deferred: 0, no_environment: 0 },
            page: { offset, limit: 20, total: 25 },
            agents: [{
              agent_id: `agent-${offset}`,
              project_id: PROJECT_ID,
              applied_revision: 0,
              state: "waiting_idle",
              observed_at: OBSERVED_AT,
            }],
          }),
        }
      }
      return undefined
    })
    renderPanel()

    expect(await screen.findByText(/1–20 of 25/)).toBeInTheDocument()
    expect(screen.getByTestId("reference-application-summary")).toHaveTextContent("25 waiting")
    await userEvent.click(screen.getByRole("button", { name: "Next agents" }))
    await waitFor(() => expect(calls.some((call) => call.path.endsWith("/references/status") && call.search.includes("offset=20"))).toBe(true))
  })
})
