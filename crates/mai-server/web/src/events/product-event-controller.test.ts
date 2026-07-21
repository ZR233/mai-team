import { QueryClient } from "@tanstack/react-query"
import { describe, expect, it } from "vitest"

import type { AgentSummary, ProductEventEnvelope, ProjectSummary } from "@/api/product-types"
import { queryKeys } from "@/api/queries"
import { applyProductEvent } from "@/events/product-event-controller"

const agent = {
  id: "agent-1",
  name: "Maintainer",
  state: { resource: "ready" },
  provider_id: "openai",
  provider_name: "OpenAI",
  model: "future-model",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
  token_usage: {
    input_tokens: 0,
    cached_input_tokens: 0,
    output_tokens: 0,
    reasoning_output_tokens: 0,
    total_tokens: 0,
  },
} satisfies AgentSummary

describe("product event cache projection", () => {
  it("applies complete entity snapshots without reloading the list", () => {
    const client = new QueryClient()
    client.setQueryData<AgentSummary[]>(queryKeys.agents, [])

    applyProductEvent(client, event("agent_created", { agent }))

    expect(client.getQueryData(queryKeys.agents)).toEqual([agent])
  })

  it("invalidates only the affected project when the event has no entity snapshot", () => {
    const client = new QueryClient()
    const first = ["projects", "project-1"] as const
    const second = ["projects", "project-2"] as const
    client.setQueryData<ProjectSummary>(first, { id: "project-1", name: "One", status: "ready", maintainer_agent_id: "a" })
    client.setQueryData<ProjectSummary>(second, { id: "project-2", name: "Two", status: "ready", maintainer_agent_id: "b" })

    applyProductEvent(client, event("project_review_queued", { project_id: "project-1" }))

    expect(client.getQueryState(first)?.isInvalidated).toBe(true)
    expect(client.getQueryState(second)?.isInvalidated).toBe(false)
  })

  it("ignores future product events without invalidating unrelated caches", () => {
    const client = new QueryClient()
    client.setQueryData(queryKeys.providers, { providers: [] })

    applyProductEvent(client, event("future_product_event", { value: 1 }))

    expect(client.getQueryState(queryKeys.providers)?.isInvalidated).toBe(false)
  })
})

function event(type: string, payload: Record<string, unknown>): ProductEventEnvelope {
  return { sequence: 1, timestamp: "2026-01-01T00:00:00Z", type, ...payload }
}
