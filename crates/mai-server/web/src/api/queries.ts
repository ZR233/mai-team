import { queryOptions } from "@tanstack/react-query"

import { api } from "@/api/client"
import type {
  AgentDetail,
  AgentSummary,
  EnvironmentDetail,
  EnvironmentSummary,
  ProjectDetail,
  ReviewRunDetail,
  ReviewRunsResponse,
  ProjectSummary,
  ProviderCatalog,
  ProvidersResponse,
  TaskSummary,
} from "@/api/product-types"

export const queryKeys = {
  agents: ["agents"] as const,
  agent: (id: string, sessionId?: string | null) => ["agents", id, sessionId ?? "selected"] as const,
  environments: ["environments"] as const,
  environment: (id: string, agentId?: string | null, sessionId?: string | null) =>
    ["environments", id, agentId ?? "root", sessionId ?? "selected"] as const,
  projects: ["projects"] as const,
  project: (id: string, agentId?: string | null, sessionId?: string | null) =>
    ["projects", id, agentId ?? "maintainer", sessionId ?? "selected"] as const,
  projectReviewRuns: (id: string) => ["projects", id, "review-runs"] as const,
  projectReviewRun: (id: string, runId: string) => ["projects", id, "review-runs", runId] as const,
  tasks: ["tasks"] as const,
  providers: ["providers"] as const,
  providerCatalog: ["provider-catalog"] as const,
  agentConfig: ["agent-config"] as const,
  skills: ["skills"] as const,
  gitAccounts: ["git-accounts"] as const,
  githubApp: ["github-app"] as const,
  relay: ["relay"] as const,
  webSearch: ["web-search"] as const,
  mcpServers: ["mcp-servers"] as const,
}

function query(params: Record<string, string | null | undefined>) {
  const search = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (value) search.set(key, value)
  }
  return search.size ? `?${search}` : ""
}

export const agentsQuery = () => queryOptions({
  queryKey: queryKeys.agents,
  queryFn: () => api<AgentSummary[]>("/agents"),
})

export const agentQuery = (id: string, sessionId?: string | null) => queryOptions({
  queryKey: queryKeys.agent(id, sessionId),
  queryFn: () => api<AgentDetail>(`/agents/${id}${query({ session_id: sessionId })}`),
  enabled: Boolean(id),
})

export const environmentsQuery = () => queryOptions({
  queryKey: queryKeys.environments,
  queryFn: () => api<EnvironmentSummary[]>("/environments"),
})

export const environmentQuery = (id: string, agentId?: string | null, sessionId?: string | null) => queryOptions({
  queryKey: queryKeys.environment(id, agentId, sessionId),
  queryFn: () => api<EnvironmentDetail>(`/environments/${id}${query({ agent_id: agentId, session_id: sessionId })}`),
  enabled: Boolean(id),
})

export const projectsQuery = () => queryOptions({
  queryKey: queryKeys.projects,
  queryFn: () => api<ProjectSummary[]>("/projects"),
})

export const projectQuery = (id: string, agentId?: string | null, sessionId?: string | null) => queryOptions({
  queryKey: queryKeys.project(id, agentId, sessionId),
  queryFn: () => api<ProjectDetail>(`/projects/${id}${query({ agent_id: agentId, session_id: sessionId })}`),
  enabled: Boolean(id),
})

export const projectReviewRunsQuery = (id: string) => queryOptions({
  queryKey: queryKeys.projectReviewRuns(id),
  queryFn: () => api<ReviewRunsResponse>(`/projects/${id}/review-runs?offset=0&limit=50`),
  enabled: Boolean(id),
})

export const projectReviewRunQuery = (projectId: string, runId?: string | null) => queryOptions({
  queryKey: queryKeys.projectReviewRun(projectId, runId || "none"),
  queryFn: () => api<ReviewRunDetail>(`/projects/${projectId}/review-runs/${runId}`),
  enabled: Boolean(projectId && runId),
})

export const tasksQuery = () => queryOptions({
  queryKey: queryKeys.tasks,
  queryFn: () => api<TaskSummary[]>("/tasks"),
})

export const providersQuery = () => queryOptions({
  queryKey: queryKeys.providers,
  queryFn: () => api<ProvidersResponse>("/providers"),
})

export const providerCatalogQuery = () => queryOptions({
  queryKey: queryKeys.providerCatalog,
  queryFn: () => api<ProviderCatalog>("/provider-catalog"),
  staleTime: Number.POSITIVE_INFINITY,
})
