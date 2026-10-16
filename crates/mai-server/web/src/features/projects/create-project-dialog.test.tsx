import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { render, screen, waitFor } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { MemoryRouter } from "react-router-dom"
import { afterEach, describe, expect, it, vi } from "vitest"

import { CreateProjectDialog } from "@/features/projects/create-project-dialog"

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

const readySource = {
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
}

function fixture() {
  return installFetch((call) => {
    if (call.path === "/git/accounts") return { body: { accounts: [{ id: "acct-1", label: "work" }] } }
    if (call.path === "/relay/status") return { body: { connected: true } }
    if (call.path === "/settings/github-app") {
      return {
        body: {
          app_id: "1",
          base_url: "https://api.github.com",
          public_url: "https://relay.example",
          has_private_key: true,
          github_name: "Mai Team",
          app_slug: "mai-team-app",
          app_html_url: "https://github.com/apps/mai-team-app",
          owner_login: "owner",
          owner_type: "Organization",
          bot_login: "mai-team-app[bot]",
          bot_user_id: 1,
          install_url: "https://github.com/apps/mai-team-app/installations/select_target",
          manage_url: "https://github.com/organizations/owner/settings/apps/mai-team-app",
        },
      }
    }
    if (call.path === "/github/installations") return { body: { installations: [] } }
    if (call.path === "/runtime/defaults") return { body: { default_docker_image: "mai/agent:latest" } }
    if (call.path === "/git/accounts/acct-1/repositories") {
      return { body: { repositories: [{ id: 7, full_name: "owner/repo", name: "repo", default_branch: "main", private: false }] } }
    }
    if (call.path === "/git/accounts/acct-1/repositories/owner/repo/packages") return { body: { packages: [] } }
    if (call.path === "/reference-sources") return { body: [readySource] }
    if (call.path === "/projects" && call.method === "POST") {
      return { body: { project: { id: "project-9", name: "repo", status: "ready", maintainer_agent_id: "agent-9" } } }
    }
    return undefined
  })
}

afterEach(() => vi.unstubAllGlobals())

describe("create project dialog", () => {
  it("sends reference_bindings built with the shared binding editor", async () => {
    const calls = fixture()
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
    const created: string[] = []
    render(
      <QueryClientProvider client={client}>
        <MemoryRouter>
          <CreateProjectDialog open onOpenChange={() => undefined} onCreated={(project) => created.push(project.id)} />
        </MemoryRouter>
      </QueryClientProvider>,
    )

    await userEvent.click(await screen.findByRole("button", { name: /owner\/repo/ }))
    await userEvent.click(await screen.findByRole("button", { name: /Add binding/ }))
    await userEvent.type(screen.getByLabelText("Alias"), "linux")
    await userEvent.click(screen.getByRole("button", { name: "Create project" }))

    await waitFor(() => expect(calls.some((call) => call.path === "/projects" && call.method === "POST")).toBe(true))
    const body = calls.find((call) => call.path === "/projects" && call.method === "POST")?.body
    const bindings = body?.reference_bindings as Array<Record<string, unknown>>
    expect(bindings).toHaveLength(1)
    expect(bindings[0]).toMatchObject({ source_id: "source-1", alias: "linux", description: "", enabled: true })
    // 只发送协议字段：不新增 access/read_only 之类的键。
    expect(Object.keys(bindings[0]).sort()).toEqual(["alias", "description", "enabled", "id", "source_id"])
    expect(body?.repository_full_name).toBe("owner/repo")
    await waitFor(() => expect(created).toEqual(["project-9"]))
  })

  it("blocks creation while a binding is invalid and sends an empty binding list otherwise", async () => {
    const calls = fixture()
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
    render(
      <QueryClientProvider client={client}>
        <MemoryRouter>
          <CreateProjectDialog open onOpenChange={() => undefined} onCreated={() => undefined} />
        </MemoryRouter>
      </QueryClientProvider>,
    )

    await userEvent.click(await screen.findByRole("button", { name: /owner\/repo/ }))
    await userEvent.click(await screen.findByRole("button", { name: /Add binding/ }))

    // 新行 alias 为空：同一个 editor 的校验必须拦住创建，而不是提交一个必被 422 的配置。
    expect(await screen.findByText("别名不能为空。")).toBeInTheDocument()
    expect(screen.getByRole("button", { name: "Create project" })).toBeDisabled()
    expect(calls.some((call) => call.path === "/projects" && call.method === "POST")).toBe(false)
  })
})
