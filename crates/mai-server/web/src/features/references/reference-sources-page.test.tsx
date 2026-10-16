import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { render, screen, waitFor } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { Toaster } from "sonner"
import { afterEach, describe, expect, it, vi } from "vitest"

import type { ReferenceSource, ReferenceSourcePolicy } from "@/api/reference-types"
import { ReferenceSourcesPage } from "@/features/references/reference-sources-page"

const policy: ReferenceSourcePolicy = {
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

function source(overrides: Partial<ReferenceSource> = {}): ReferenceSource {
  return {
    id: "source-1",
    name: "Linux reference",
    revision_label: "v6.12",
    host_path: "/srv/mai-references/linux/repo",
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
    ...overrides,
  }
}

interface Call {
  path: string
  method: string
  body: Record<string, unknown> | null
}

function installFetch(handle: (call: Call) => { status?: number; body?: unknown } | undefined) {
  const calls: Call[] = []
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input), "http://127.0.0.1")
    const call: Call = {
      path: url.pathname,
      method: (init?.method ?? "GET").toUpperCase(),
      body: init?.body ? JSON.parse(String(init.body)) as Record<string, unknown> : null,
    }
    calls.push(call)
    const result = handle(call)
    if (!result) return new Response("not found", { status: 404 })
    return new Response(result.body === undefined ? "" : JSON.stringify(result.body), {
      status: result.status ?? 200,
      headers: { "content-type": "application/json" },
    })
  }))
  return calls
}

function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  render(
    <QueryClientProvider client={client}>
      <ReferenceSourcesPage />
      <Toaster />
    </QueryClientProvider>,
  )
  return client
}

afterEach(() => vi.unstubAllGlobals())

describe("reference sources settings page", () => {
  it("renders an invalid source and untrusted HTML name as text", async () => {
    const name = '<img src=x onerror="window.__maiReferenceXss=1">'
    installFetch((call) => {
      if (call.path === "/reference-source-policy") return { body: policy }
      if (call.path === "/reference-sources") return { body: [source({
        name, state: "invalid", manifest_digest: null,
        validation_error: { code: "unsafe_symlink", message: "源包含外逃链接", observed_at: "2026-10-15T00:00:00Z" },
      })] }
      return undefined
    })
    renderPage()
    expect(await screen.findByText(name)).toBeInTheDocument()
    expect(screen.getByText("源包含外逃链接")).toBeInTheDocument()
    expect(document.querySelector('img[src="x"]')).toBeNull()
    expect(screen.queryByText("scanning")).not.toBeInTheDocument()
  })
  it("registers a source only after the access confirmation and sends just the protocol fields", async () => {
    let sources: ReferenceSource[] = []
    const calls = installFetch((call) => {
      if (call.path === "/reference-source-policy") return { body: policy }
      if (call.path === "/reference-sources" && call.method === "GET") return { body: sources }
      if (call.path === "/reference-sources" && call.method === "POST") {
        sources = [source({ state: "validating", manifest_digest: null, file_count: 0, byte_count: 0 })]
        return { status: 202, body: sources[0] }
      }
      return undefined
    })
    renderPage()

    await userEvent.click(await screen.findByRole("button", { name: /Register source/ }))
    await userEvent.type(screen.getByLabelText("Name"), "Linux reference")
    await userEvent.type(screen.getByLabelText("Host path (server)"), "/srv/mai-references/linux/repo")
    await userEvent.type(screen.getByLabelText("Version label"), "v6.12")

    await userEvent.click(screen.getByRole("button", { name: /Register and verify/ }))
    expect(calls.some((call) => call.method === "POST")).toBe(false)
    expect(await screen.findByText(/确认该目录适合提供给 Agent 与模型访问/)).toBeInTheDocument()

    await userEvent.click(screen.getByRole("switch", { name: /suitable to expose to agents and models/ }))
    await userEvent.click(screen.getByRole("button", { name: /Register and verify/ }))

    await waitFor(() => expect(calls.some((call) => call.method === "POST")).toBe(true))
    const post = calls.find((call) => call.method === "POST")
    expect(post?.body).toEqual({
      name: "Linux reference",
      host_path: "/srv/mai-references/linux/repo",
      revision_label: "v6.12",
      approved_for_agent_access: true,
    })
    expect(Object.keys(post?.body ?? {}).sort()).toEqual([
      "approved_for_agent_access",
      "host_path",
      "name",
      "revision_label",
    ])
  })

  it("shows asynchronous validation progress and queues a re-verification", async () => {
    const calls = installFetch((call) => {
      if (call.path === "/reference-source-policy") return { body: policy }
      if (call.path === "/reference-sources/source-1/verify") {
        return { status: 202, body: source({ state: "validating", manifest_digest: null }) }
      }
      if (call.path === "/reference-sources") return { body: [source({ state: "validating", manifest_digest: null })] }
      return undefined
    })
    renderPage()

    expect(await screen.findByText("scanning")).toBeInTheDocument()
    await userEvent.click(screen.getByRole("button", { name: /Verify/ }))
    await waitFor(() => expect(calls.some((call) => call.path === "/reference-sources/source-1/verify")).toBe(true))
  })

  it("reports a source that is still in use instead of unregistering it", async () => {
    installFetch((call) => {
      if (call.path === "/reference-source-policy") return { body: policy }
      if (call.path === "/reference-sources/source-1" && call.method === "DELETE") {
        return {
          status: 409,
          body: {
            code: "source_in_use",
            message: "该参考源仍被项目引用，请先从项目解绑",
            field_errors: [{ field: "projects", message: "project-1" }],
            request_id: "req-9",
            retryable: false,
          },
        }
      }
      if (call.path === "/reference-sources") return { body: [source()] }
      return undefined
    })
    renderPage()

    await userEvent.click(await screen.findByRole("button", { name: /Unregister/ }))
    await userEvent.click(screen.getByRole("button", { name: "Confirm unregister Linux reference" }))

    expect(await screen.findByText("该参考源仍被项目引用，请先从项目解绑")).toBeInTheDocument()
    expect(screen.getByText("Linux reference")).toBeInTheDocument()
  })

  it("patches only the editable metadata fields", async () => {
    const calls = installFetch((call) => {
      if (call.path === "/reference-source-policy") return { body: policy }
      if (call.path === "/reference-sources/source-1" && call.method === "PATCH") {
        return { body: source({ name: "Linux 6.12" }) }
      }
      if (call.path === "/reference-sources") return { body: [source()] }
      return undefined
    })
    renderPage()

    await userEvent.click(await screen.findByRole("button", { name: "Edit" }))
    const name = screen.getByLabelText("Name")
    await userEvent.clear(name)
    await userEvent.type(name, "Linux 6.12")
    await userEvent.click(screen.getByRole("button", { name: /Save metadata/ }))

    await waitFor(() => expect(calls.some((call) => call.method === "PATCH")).toBe(true))
    expect(calls.find((call) => call.method === "PATCH")?.body).toEqual({
      name: "Linux 6.12",
      revision_label: "v6.12",
      approved_for_agent_access: true,
    })
  })
})
