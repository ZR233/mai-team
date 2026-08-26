import type { AgentSummary } from "@/api/product-types"

export function agentCanRunThread(agent: AgentSummary): boolean {
  if (agent.resource.state !== "ready" || !agent.runtime) return false
  switch (agent.runtime.state.kind) {
    case "idle":
    case "queued":
    case "running":
    case "waitingTool":
    case "waitingInteraction":
      return true
    case "cancelling":
    case "closing":
    case "closed":
    case "faulted":
      return false
  }
}

export function agentPresentationStatus(agent: AgentSummary, turnStatus?: string | null): string {
  if (agent.resource.state !== "ready") return agent.resource.state
  if (!agent.runtime) return "unavailable"
  switch (agent.runtime.state.kind) {
    case "idle":
    case "queued":
    case "running":
    case "waitingTool":
    case "waitingInteraction":
    case "cancelling":
      return turnStatus || agent.runtime.state.kind
    case "closing":
    case "closed":
    case "faulted":
      return agent.runtime.state.kind
  }
}
