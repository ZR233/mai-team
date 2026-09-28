import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { render, screen } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { afterEach, describe, expect, it, vi } from "vitest"

import { SidebarProvider } from "@/components/ui/sidebar"
import ProvidersPage from "@/features/providers/providers-page"

const catalog = {
  schema_version: 10,
  revision: "future-revision",
  presets: [{
    id: "future-provider",
    display_name: "Future Cloud",
    description: "A provider unknown to this UI build",
    base_url: "https://future.invalid/v1",
    suggested_model: "future-model",
    model_catalog_id: "future-catalog",
    icon_key: "unknown-future-icon",
    pricing_enabled: false,
    credential: { label: "Future token", env_var: "FUTURE_TOKEN" },
    service_capabilities: {
      web_search: { hosted_responses: false, hosted_dialect: "", standalone: null },
      prompt_cache_dialect: "",
      responses_programmatic_tool_calling: false,
    },
  }],
  model_catalogs: {
    "future-catalog": {
      id: "future-catalog",
      models: [{
        id: "future-model",
        display_name: "Future Model",
        transport: {
          protocol: "responses",
          connection_modes: [{ id: "http", display_name: "HTTP" }],
          default_connection_mode: "http",
        },
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
  it("renders a configured PL model from binding.transport", async () => {
    const provider = {
      id: "future-provider",
      config: { name: "Future Cloud", base_url: "https://future.invalid/v1" },
      has_api_key: true,
      has_http_headers: false,
      models: [{
        slug: "future-model",
        display_name: "Future Model",
        binding: {
          transport: {
            protocol: "responses",
            supported_connection_modes: ["web_socket", "http"],
            default_connection_mode: "web_socket",
          },
          request: { protocol: { api: "responses" } },
        },
        context_window: 1000000,
        max_output_tokens: 128000,
        capabilities: {
          input: [{ modality: "text" }, { modality: "image" }],
          output: ["text"],
          streaming: true,
          temperature: false,
          reasoning: true,
          web_search: true,
          tools: { function_calling: true, parallel_tool_calls: true, custom_tools: false, freeform_tools: false, programmatic_tool_calling: false },
        },
        pricing: { kind: "unknown" },
      }],
    }
    vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => new Response(
      JSON.stringify(String(input).endsWith("/provider-catalog") ? catalog : { providers: [provider] }),
      { status: 200, headers: { "content-type": "application/json" } },
    )))
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
    render(<QueryClientProvider client={client}><SidebarProvider><ProvidersPage /></SidebarProvider></QueryClientProvider>)

    expect((await screen.findAllByText("Future Cloud"))[0]).toBeVisible()
    expect(screen.getAllByText("responses")[0]).toBeVisible()
    expect(screen.getAllByText("web_socket")[0]).toBeVisible()
    expect(screen.getAllByText("future-model")[0]).toBeVisible()
    await userEvent.click(screen.getAllByRole("button", { name: "View Future Cloud models" })[0])
    expect(screen.getByText("Future Model")).toBeVisible()
    expect(screen.getByText("1,000,000")).toBeVisible()
    expect(screen.getByText(/Input: text, image/)).toBeVisible()
  })

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
