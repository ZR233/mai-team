import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { render, screen } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { afterEach, describe, expect, it, vi } from "vitest"

import { SidebarProvider } from "@/components/ui/sidebar"
import ProvidersPage from "@/features/providers/providers-page"

const catalog = {
  schema_version: 4,
  revision: "future-revision",
  presets: [{
    id: "future-provider",
    display_name: "Future Cloud",
    description: "A provider unknown to this UI build",
    base_url: "https://future.invalid/v1",
    suggested_model: "future-model",
    model_catalog_id: "future-catalog",
    icon_key: "unknown-future-icon",
    credential: { label: "Future token", env_var: "FUTURE_TOKEN" },
    transport: {
      protocol: "responses",
      connection_modes: [
        { id: "web_socket", display_name: "WebSocket" },
        { id: "http", display_name: "HTTP" },
      ],
      default_connection_mode: "web_socket",
    },
  }],
  model_catalogs: {
    "future-catalog": {
      models: [{
        id: "future-model",
        display_name: "Future Model",
        reasoning: {
          default_variant: "balanced",
          variants: [
            { id: "eco", label: "Eco" },
            { id: "balanced", label: "Balanced" },
            { id: "max", label: "Maximum" },
          ],
        },
      }],
    },
  },
}

afterEach(() => vi.unstubAllGlobals())

describe("provider catalog driven editor", () => {
  it("submits a future preset as instance overrides without rebuilding PL semantics", async () => {
    let saved: unknown
    vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = String(input)
      if (init?.method === "PUT") saved = JSON.parse(String(init.body)) as unknown
      return new Response(JSON.stringify(path.endsWith("/provider-catalog") ? catalog : { providers: [] }), {
        status: 200,
        headers: { "content-type": "application/json" },
      })
    }))
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
    render(<QueryClientProvider client={client}><SidebarProvider><ProvidersPage /></SidebarProvider></QueryClientProvider>)

    const addButtons = await screen.findAllByRole("button", { name: /add provider/i })
    await userEvent.click(addButtons[0])

    expect(await screen.findByText("Future Cloud")).toBeInTheDocument()
    expect(screen.getByLabelText("Provider ID")).toHaveValue("future-provider")
    expect(screen.getByLabelText("Display name")).toHaveValue("Future Cloud")
    expect(screen.getByLabelText("Base URL override")).toHaveValue("https://future.invalid/v1")
    expect(screen.queryByText("Connection mode")).not.toBeInTheDocument()

    await userEvent.click(screen.getByRole("button", { name: "Save provider" }))

    expect(saved).toEqual({
      providers: [{
        id: "future-provider",
        source: "preset",
        preset_id: "future-provider",
        name: "Future Cloud",
        base_url: "https://future.invalid/v1",
        bearer_token_env: "FUTURE_TOKEN",
      }],
    })
  })
})
