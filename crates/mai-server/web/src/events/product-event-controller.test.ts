import { QueryClient } from "@tanstack/react-query"
import { describe, expect, it } from "vitest"

import type { AgentSummary, ProductEventEnvelope, ProjectSummary } from "@/api/product-types"
import { queryKeys } from "@/api/queries"
import { applyProductEvent } from "@/events/product-event-controller"

const agent = {
  id: "agent-1",
  name: "Maintainer",
  resource: { state: "ready", error: null },
  provider_id: "openai",
  provider_name: "OpenAI",
  model: "future-model",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
  usage: {
    promptTokens: 0,
    cachedPromptTokens: 0,
    cacheMissTokens: 0,
    cacheWriteTokens: 0,
    hasIncompleteUsage: false,
    completionTokens: 0,
    reasoningTokens: 0,
    totalTokens: 0,
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

  it("invalidates only the discovery snapshot for a discovery update", () => {
    const client = new QueryClient()
    const discovery = queryKeys.projectReviewDiscovery("project-1")
    const reviewJobs = queryKeys.projectPullRequestReviews("project-1")
    client.setQueryData(discovery, { state: "scanning" })
    client.setQueryData(reviewJobs, { reviews: [] })

    applyProductEvent(client, event("project_review_discovery_updated", { project_id: "project-1" }))

    expect(client.getQueryState(discovery)?.isInvalidated).toBe(true)
    expect(client.getQueryState(reviewJobs)?.isInvalidated).toBe(false)
  })

  it("ignores future product events without invalidating unrelated caches", () => {
    const client = new QueryClient()
    client.setQueryData(queryKeys.providers, { providers: [] })

    applyProductEvent(client, event("future_product_event", { value: 1 }))

    expect(client.getQueryState(queryKeys.providers)?.isInvalidated).toBe(false)
  })

  it("invalidates only the affected project references for a references change", () => {
    const client = new QueryClient()
    const first = queryKeys.projectReferences("project-1")
    const firstStatus = queryKeys.projectReferencesStatus("project-1")
    const second = queryKeys.projectReferences("project-2")
    client.setQueryData(first, { configuration: { revision: 0, bindings: [] } })
    client.setQueryData(firstStatus, { state: "pending" })
    client.setQueryData(second, { configuration: { revision: 0, bindings: [] } })

    applyProductEvent(client, event("project_references_changed", { project_id: "project-1", revision: 2 }))

    expect(client.getQueryState(first)?.isInvalidated).toBe(true)
    expect(client.getQueryState(firstStatus)?.isInvalidated).toBe(true)
    expect(client.getQueryState(second)?.isInvalidated).toBe(false)
  })

  it("invalidates the application status without touching the durable configuration", () => {
    const client = new QueryClient()
    const configuration = queryKeys.projectReferences("project-1")
    const status = queryKeys.projectReferencesStatus("project-1")
    client.setQueryData(configuration, { configuration: { revision: 1, bindings: [] } })
    client.setQueryData(status, { state: "pending" })

    applyProductEvent(client, event("agent_reference_application_changed", {
      project_id: "project-1",
      agent_id: "agent-1",
      operation_id: "op-1",
      state: "applied",
    }))

    expect(client.getQueryState(status)?.isInvalidated).toBe(true)
    expect(client.getQueryState(configuration)?.isInvalidated).toBe(false)
  })

  it("invalidates the registered source list for a source change", () => {
    const client = new QueryClient()
    const list = queryKeys.referenceSources
    const projects = queryKeys.projects
    client.setQueryData(list, [])
    client.setQueryData(projects, [])

    applyProductEvent(client, event("reference_source_changed", { source_id: "source-1", state: "ready" }))

    expect(client.getQueryState(list)?.isInvalidated).toBe(true)
    expect(client.getQueryState(projects)?.isInvalidated).toBe(false)
  })
})

function event(type: string, payload: Record<string, unknown>): ProductEventEnvelope {
  return { sequence: 1, timestamp: "2026-01-01T00:00:00Z", type, ...payload }
}
