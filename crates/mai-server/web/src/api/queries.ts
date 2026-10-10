import { infiniteQueryOptions, queryOptions } from "@tanstack/react-query"

import { api } from "@/api/client"
import type { ThreadTurnHistory, ThreadTurnPage } from "@/events/thread-events.generated"
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
  ReviewInferenceBillingPage,
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
  threadTurns: (threadId: string) => ["threads", threadId, "turns"] as const,
  activeThreadTurn: (threadId: string, turnId: string) => ["threads", threadId, "active-turn", turnId] as const,
  environments: ["environments"] as const,
  environment: (id: string) => ["environments", id] as const,
  projects: ["projects"] as const,
  project: (id: string) => ["projects", id] as const,
  projectReviewRuns: (id: string) => ["projects", id, "review-runs"] as const,
  projectReviewDiscovery: (id: string) => ["projects", id, "review-discovery"] as const,
  projectReviewRun: (id: string, runId: string) => ["projects", id, "review-runs", runId] as const,
  projectReviewRunBilling: (id: string, runId: string) => ["projects", id, "review-runs", runId, "billing"] as const,
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

/**
 * 用产品 `/threads/{id}/turns` 分页读取一个 Thread 的历史 Turn/items。
 *
 * 每页按 Turn 终态从新到旧返回，`nextCursor` 是本页最旧 Turn 的排他上界；把它原样
 * 传回即可继续读取更早的一页。Thread 首帧不再携带历史 items，调用方负责按时间线顺序
 * 归并 `pages[].turns[].items`。
 */
export const threadTurnsQuery = (threadId: string) => infiniteQueryOptions({
  queryKey: queryKeys.threadTurns(threadId),
  queryFn: ({ pageParam }) => api<ThreadTurnPage>(`/threads/${encodeURIComponent(threadId)}/turns${query({ cursor: pageParam })}`),
  initialPageParam: undefined as string | undefined,
  getNextPageParam: (lastPage) => lastPage.nextCursor,
  enabled: Boolean(threadId),
})

export const activeThreadTurnQuery = (threadId: string, turnId: string | null) => queryOptions({
  queryKey: queryKeys.activeThreadTurn(threadId, turnId || "none"),
  queryFn: () => api<ThreadTurnHistory | null>(`/threads/${encodeURIComponent(threadId)}/active-turn`),
  enabled: Boolean(threadId && turnId),
  refetchInterval: 5_000,
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

export const projectQuery = (id: string) => queryOptions({
  queryKey: queryKeys.project(id),
  queryFn: () => api<ProjectDetail>(`/projects/${id}`),
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
  refetchInterval: (query) => ["syncing", "running"].includes(query.state.data?.status ?? "") ? 5_000 : false,
})

export const projectReviewRunBillingQuery = (projectId: string, runId: string | null, enabled = true, active = false) => infiniteQueryOptions({
  queryKey: queryKeys.projectReviewRunBilling(projectId, runId || "none"),
  queryFn: ({ pageParam }) => api<ReviewInferenceBillingPage>(`/projects/${projectId}/review-runs/${runId}/billing${query({ before_sequence: pageParam ? String(pageParam) : undefined, limit: "100" })}`),
  initialPageParam: undefined as number | undefined,
  getNextPageParam: (lastPage) => lastPage.nextBeforeSequence ?? undefined,
  enabled: Boolean(projectId && runId && enabled),
  refetchInterval: active ? 5_000 : false,
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
  refetchInterval: (query) => query.state.data?.items.some((item) => ["queued", "preparing", "running", "retry_waiting", "submission_pending", "reconciling"].includes(item.job.status)) ? 5_000 : false,
})

export const projectReviewJobQuery = (projectId: string, jobId?: string | null) => queryOptions({
  queryKey: queryKeys.projectReviewJob(projectId, jobId || "none"),
  queryFn: () => api<ReviewJobDetail>(`/projects/${projectId}/review-jobs/${jobId}`),
  enabled: Boolean(projectId && jobId),
  refetchInterval: (query) => ["queued", "preparing", "running", "retry_waiting", "submission_pending", "reconciling"].includes(query.state.data?.status ?? "") ? 5_000 : false,
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
