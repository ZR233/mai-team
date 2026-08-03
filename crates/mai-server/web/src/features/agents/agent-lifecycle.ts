import type { AgentSummary } from "@/api/product-types"

export function agentCanRunSessionTurn(agent: AgentSummary): boolean {
  const lifecycle = agent.state.runtime?.lifecycle
  return agent.state.resource === "ready" && (!lifecycle || lifecycle === "active")
}

export function agentPresentationStatus(agent: AgentSummary, turnStatus?: string | null): string {
  if (agent.state.resource !== "ready") return agent.state.resource
  const runtime = agent.state.runtime
  if (runtime && runtime.lifecycle !== "active") return runtime.lifecycle
  return turnStatus || runtime?.activity || agent.state.resource
}
