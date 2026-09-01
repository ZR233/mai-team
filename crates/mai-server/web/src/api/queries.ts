import { queryOptions } from "@tanstack/react-query"

import { api } from "@/api/client"
import type {
  AgentDetail,
  AgentSummary,
  EnvironmentDetail,
  EnvironmentSummary,
  PullRequestReviewHistoryPage,
  PullRequestReviewPage,
  ProjectDetail,
  ProjectReviewDiscoverySnapshot,
  ReviewJobDetail,
  ReviewRunDetail,
  ReviewRunsResponse,
  ProjectSummary,
  ProviderCatalog,
  ProvidersResponse,
  TaskSummary,
} from "@/api/product-types"

export const queryKeys = {
  agents: ["agents"] as const,
  agent: (id: string) => ["agents", id] as const,
  environments: ["environments"] as const,
  environment: (id: string) => ["environments", id] as const,
  projects: ["projects"] as const,
  project: (id: string, agentId?: string | null) => ["projects", id, agentId ?? "maintainer"] as const,
  projectReviewRuns: (id: string) => ["projects", id, "review-runs"] as const,
  projectReviewDiscovery: (id: string) => ["projects", id, "review-discovery"] as const,
  projectReviewRun: (id: string, runId: string) => ["projects", id, "review-runs", runId] as const,
  projectPullRequestReviews: (id: string) => ["projects", id, "pull-request-reviews"] as const,
  projectPullRequestReviewPage: (id: string, page: number, pageSize: number) => ["projects", id, "pull-request-reviews", page, pageSize] as const,
  projectPullRequestReviewHistory: (id: string, pr: number) => ["projects", id, "pull-request-reviews", pr, "history"] as const,
  projectPullRequestReviewHistoryPage: (id: string, pr: number, page: number, pageSize: number) => ["projects", id, "pull-request-reviews", pr, "history", page, pageSize] as const,
  projectReviewJob: (id: string, jobId: string) => ["projects", id, "review-jobs", jobId] as const,
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

export const agentQuery = (id: string) => queryOptions({
  queryKey: queryKeys.agent(id),
  queryFn: () => api<AgentDetail>(`/agents/${id}`),
  enabled: Boolean(id),
})

export const environmentsQuery = () => queryOptions({
  queryKey: queryKeys.environments,
  queryFn: () => api<EnvironmentSummary[]>("/environments"),
})

export const environmentQuery = (id: string) => queryOptions({
  queryKey: queryKeys.environment(id),
  queryFn: () => api<EnvironmentDetail>(`/environments/${id}`),
  enabled: Boolean(id),
})

export const projectsQuery = () => queryOptions({
  queryKey: queryKeys.projects,
  queryFn: () => api<ProjectSummary[]>("/projects"),
})

export const projectQuery = (id: string, agentId?: string | null) => queryOptions({
  queryKey: queryKeys.project(id, agentId),
  queryFn: () => api<ProjectDetail>(`/projects/${id}${query({ agent_id: agentId })}`),
  enabled: Boolean(id),
})

export const projectReviewRunsQuery = (id: string) => queryOptions({
  queryKey: queryKeys.projectReviewRuns(id),
  queryFn: () => api<ReviewRunsResponse>(`/projects/${id}/review-runs?offset=0&limit=50`),
  enabled: Boolean(id),
})

export const projectReviewDiscoveryQuery = (id: string) => queryOptions({
  queryKey: queryKeys.projectReviewDiscovery(id),
  queryFn: () => api<ProjectReviewDiscoverySnapshot>(`/projects/${id}/review-discovery`),
  enabled: Boolean(id),
  refetchInterval: 60_000,
})

export const projectReviewRunQuery = (projectId: string, runId?: string | null) => queryOptions({
  queryKey: queryKeys.projectReviewRun(projectId, runId || "none"),
  queryFn: () => api<ReviewRunDetail>(`/projects/${projectId}/review-runs/${runId}`),
  enabled: Boolean(projectId && runId),
})

export const projectPullRequestReviewsQuery = (id: string, page: number, pageSize = 20) => queryOptions({
  queryKey: queryKeys.projectPullRequestReviewPage(id, page, pageSize),
  queryFn: () => api<PullRequestReviewPage>(`/projects/${id}/pull-request-reviews${query({ page: String(page), page_size: String(pageSize) })}`),
  enabled: Boolean(id),
})

export const projectPullRequestReviewHistoryQuery = (id: string, pr: number, page: number, pageSize = 20) => queryOptions({
  queryKey: queryKeys.projectPullRequestReviewHistoryPage(id, pr, page, pageSize),
  queryFn: () => api<PullRequestReviewHistoryPage>(`/projects/${id}/pull-request-reviews/${pr}/history${query({ page: String(page), page_size: String(pageSize) })}`),
  enabled: Boolean(id && pr),
})

export const projectReviewJobQuery = (projectId: string, jobId?: string | null) => queryOptions({
  queryKey: queryKeys.projectReviewJob(projectId, jobId || "none"),
  queryFn: () => api<ReviewJobDetail>(`/projects/${projectId}/review-jobs/${jobId}`),
  enabled: Boolean(projectId && jobId),
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
