import { type QueryClient, useQueryClient } from "@tanstack/react-query"
import { useEffect } from "react"

import type { AgentSummary, ProductEventEnvelope, ProjectSummary, TaskSummary } from "@/api/product-types"
import { queryKeys } from "@/api/queries"

function replaceEntity<T extends { id: string }>(current: T[] | undefined, entity: T): T[] {
  if (!current) return [entity]
  const exists = current.some((candidate) => candidate.id === entity.id)
  return exists
    ? current.map((candidate) => candidate.id === entity.id ? entity : candidate)
    : [...current, entity]
}

export function ProductEventController() {
  const queryClient = useQueryClient()

  useEffect(() => {
    const source = new EventSource("/events/product")
    const onEvent = (message: MessageEvent<string>) => {
      let event: ProductEventEnvelope
      try {
        event = JSON.parse(message.data) as ProductEventEnvelope
      } catch {
        return
      }
      applyProductEvent(queryClient, event)
    }

    for (const name of PRODUCT_EVENT_NAMES) source.addEventListener(name, onEvent as EventListener)
    return () => source.close()
  }, [queryClient])

  return null
}

export function applyProductEvent(queryClient: QueryClient, event: ProductEventEnvelope) {
  switch (event.type) {
        case "agent_created":
        case "agent_updated": {
          const agent = event.agent as AgentSummary
          queryClient.setQueryData<AgentSummary[]>(queryKeys.agents, (current) => replaceEntity(current, agent))
          queryClient.invalidateQueries({ queryKey: ["agents", agent.id] })
          if (agent.project_id) queryClient.invalidateQueries({ queryKey: ["projects", agent.project_id] })
          if (agent.task_id) queryClient.invalidateQueries({ queryKey: ["tasks", agent.task_id] })
          break
        }
        case "agent_deleted": {
          const id = String(event.agent_id)
          queryClient.setQueryData<AgentSummary[]>(queryKeys.agents, (current) => current?.filter((agent) => agent.id !== id))
          queryClient.removeQueries({ queryKey: ["agents", id] })
          break
        }
        case "project_created":
        case "project_updated": {
          const project = event.project as ProjectSummary
          queryClient.setQueryData<ProjectSummary[]>(queryKeys.projects, (current) => replaceEntity(current, project))
          queryClient.invalidateQueries({ queryKey: ["projects", project.id], exact: false })
          break
        }
        case "project_deleted": {
          const id = String(event.project_id)
          queryClient.setQueryData<ProjectSummary[]>(queryKeys.projects, (current) => current?.filter((project) => project.id !== id))
          queryClient.removeQueries({ queryKey: ["projects", id] })
          break
        }
        case "task_created":
        case "task_updated": {
          const task = event.task as TaskSummary
          queryClient.setQueryData<TaskSummary[]>(queryKeys.tasks, (current) => replaceEntity(current, task))
          queryClient.invalidateQueries({ queryKey: ["tasks", task.id], exact: false })
          break
        }
        case "task_deleted": {
          const id = String(event.task_id)
          queryClient.setQueryData<TaskSummary[]>(queryKeys.tasks, (current) => current?.filter((task) => task.id !== id))
          queryClient.removeQueries({ queryKey: ["tasks", id] })
          break
        }
        case "plan_updated":
          queryClient.invalidateQueries({ queryKey: ["tasks", String(event.task_id)], exact: false })
          break
        case "project_review_queued":
          queryClient.invalidateQueries({ queryKey: ["projects", String(event.project_id)], exact: false })
          break
        case "mcp_server_status_changed":
          queryClient.invalidateQueries({ queryKey: queryKeys.mcpServers })
          break
        case "github_webhook_received":
        case "operation_failed":
        case "artifact_created":
          break
        default:
          break
  }
}

export const PRODUCT_EVENT_NAMES = [
  "agent_created", "agent_updated", "agent_deleted", "task_created", "task_updated",
  "task_deleted", "project_created", "project_updated", "project_deleted",
  "github_webhook_received", "project_review_queued", "operation_failed", "plan_updated",
  "artifact_created", "mcp_server_status_changed",
] as const
