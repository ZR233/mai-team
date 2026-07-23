import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { render, screen, waitFor } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { afterEach, describe, expect, it, vi } from "vitest"

import { GithubAppSection } from "@/features/settings/sections/github-app-section"

interface SavedRequest {
  path: string
  body: Record<string, unknown>
}

const relaySettings = {
  enabled: true,
  url: "https://relay.example",
  has_token: true,
  node_id: "node-a",
}

const githubAppSettings = {
  app_id: "123",
  base_url: "https://api.github.com",
  public_url: "https://relay.example",
  has_private_key: true,
  app_slug: "mai",
  install_url: "https://github.com/apps/mai/installations/select_target",
}

afterEach(() => vi.unstubAllGlobals())

describe("GitHub App settings", () => {
  it("submits other settings without blank secret fields", async () => {
    const saved = stubSettingsApi()
    renderSettings()

    expect(await screen.findByText("Relay connection")).toBeInTheDocument()
    await userEvent.click(screen.getByRole("button", { name: "Save relay" }))
    await userEvent.click(screen.getByRole("button", { name: "Save app" }))

    await waitFor(() => expect(saved).toHaveLength(2))
    expect(saved).toEqual([
      {
        path: "/settings/relay",
        body: {
          enabled: true,
          url: "https://relay.example",
          node_id: "node-a",
        },
      },
      {
        path: "/settings/github-app",
        body: {
          public_url: "https://relay.example",
          base_url: "https://api.github.com",
          app_id: "123",
          app_slug: "mai",
        },
      },
    ])
  })

  it("submits entered secrets and clears their inputs after saving", async () => {
    const saved = stubSettingsApi()
    renderSettings()

    expect(await screen.findByText("Relay connection")).toBeInTheDocument()
    const relayToken = screen.getByLabelText("Relay token")
    const privateKey = screen.getByLabelText("PEM private key")
    await userEvent.type(relayToken, "new-relay-token")
    await userEvent.type(privateKey, "new-private-key")
    await userEvent.click(screen.getByRole("button", { name: "Save relay" }))
    await userEvent.click(screen.getByRole("button", { name: "Save app" }))

    await waitFor(() => expect(saved).toHaveLength(2))
    expect(saved[0]?.body.token).toBe("new-relay-token")
    expect(saved[1]?.body.private_key).toBe("new-private-key")
    await waitFor(() => {
      expect(relayToken).toHaveValue("")
      expect(privateKey).toHaveValue("")
    })
  })
})

function renderSettings() {
  const client = new QueryClient({
    defaultOptions: {
      queries: { retry: false },
      mutations: { retry: false },
    },
  })
  render(
    <QueryClientProvider client={client}>
      <GithubAppSection />
    </QueryClientProvider>,
  )
}

function stubSettingsApi() {
  const saved: SavedRequest[] = []
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const path = String(input)
    if (init?.method === "PUT") {
      saved.push({
        path,
        body: JSON.parse(String(init.body)) as Record<string, unknown>,
      })
    }
    return jsonResponse(responseFor(path))
  }))
  return saved
}

function responseFor(path: string) {
  if (path === "/relay/status") {
    return {
      enabled: true,
      connected: true,
      relay_url: relaySettings.url,
      node_id: relaySettings.node_id,
    }
  }
  if (path === "/settings/relay") return relaySettings
  if (path === "/settings/github-app") return githubAppSettings
  if (path === "/github/installations") return { installations: [] }
  if (path === "/relay/update") {
    return {
      current_version: "0.1.0",
      latest_version: "0.1.0",
      has_update: false,
      can_update: true,
    }
  }
  throw new Error(`Unexpected request: ${path}`)
}

function jsonResponse(value: unknown) {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { "content-type": "application/json" },
  })
}
