export type Id = string

import type { Thread, ThreadTurnHistory } from "@/events/thread-events.generated"

export interface TokenUsage {
  promptTokens: number
  cachedPromptTokens: number
  cacheWriteTokens: number
  completionTokens: number
  reasoningTokens: number
  totalTokens: number
}

export type AgentRuntimeState =
  | { kind: "idle"; data: null }
  | { kind: "queued"; data: { turnId: Id } }
  | { kind: "running"; data: { turnId: Id } }
  | { kind: "waitingTool"; data: { turnId: Id } }
  | { kind: "waitingInteraction"; data: { turnId: Id; interactionId: string } }
  | { kind: "cancelling"; data: { turnId: Id } }
  | { kind: "closing"; data: null }
  | { kind: "closed"; data: null }
  | {
    kind: "faulted"
    data: {
      error: { code: string; message: string; retryable: boolean }
      turnId?: Id | null
      classification: "recoverableRuntime" | "recoverableProtocol" | "aggregateCorruption" | "legacyUnknown"
    }
  }

export interface AgentRuntimeSnapshot {
  identity: { id: Id; parentId?: Id | null; role: string; depth: number }
  state: AgentRuntimeState
  pendingInputs: number
  progress?: Record<string, unknown> | null
  lastTurn?: Record<string, unknown> | null
  revision: number
  eventSequence: number
  updatedAt: number
}

export interface AgentSummary {
  id: Id
  parent_id?: Id | null
  task_id?: Id | null
  project_id?: Id | null
  role?: string | null
  name: string
  resource: {
    state: "provisioning" | "ready" | "deleting" | "failed" | "deleted"
    error?: string | null
  }
  runtime?: AgentRuntimeSnapshot | null
  container_id?: string | null
  docker_image?: string
  provider_id: string
  provider_name: string
  model: string
  reasoning_effort?: string | null
  created_at: string
  updated_at: string
  token_usage: TokenUsage
}

export interface AgentDetail extends AgentSummary {
  thread: Thread
}

export interface EnvironmentSummary {
  id: Id
  title?: string
  name?: string
  status?: string
  root_agent?: AgentSummary
  [key: string]: unknown
}

export interface EnvironmentDetail extends EnvironmentSummary {
  root_agent: AgentDetail
}

export interface ProjectSummary {
  id: Id
  name: string
  repository?: string
  repository_full_name?: string
  status: string
  maintainer_agent_id: Id
  review?: Record<string, unknown>
  [key: string]: unknown
}

export type ReviewRunOutcome = "review_submitted" | "no_eligible_pr" | "failed"
export type ReviewDecision = "approve" | "request_changes" | "comment"
export type ReviewHistoryStatus = "available" | "pl_v2_archived"

export interface ReviewRunSummary {
  id: Id
  job_id?: Id | null
  attempt_index?: number
  status: string
  pr?: number | null
  summary?: string | null
  error?: string | null
  started_at: string
  finished_at?: string | null
  outcome?: ReviewRunOutcome | null
  review_event?: ReviewDecision | null
  reviewer_agent_id?: string | null
  turn_id?: string | null
  token_usage?: TokenUsage
  history_status: ReviewHistoryStatus
  history_archive_id?: string | null
  history_archived_at?: string | null
  [key: string]: unknown
}

export type ReviewJobStatus = "queued" | "preparing" | "running" | "retry_waiting" | "submission_pending" | "reconciling" | "succeeded" | "failed" | "cancelled" | "superseded" | "skipped"
export type ReviewSkipReason = "pull_request_closed" | "draft" | "ci_pending" | "already_reviewed_current_head"

export interface ReviewFailure {
  category: string
  code?: string | null
  http_status?: number | null
  message: string
  retry: unknown
}

export interface ReviewEnvironmentWarning {
  code: string
  image: string
  cached_image_id: string
  message: string
  observed_at: string
}

export interface ReviewSubmissionReceipt {
  github_review_id: number
  event: ReviewDecision
  head_sha: string
  html_url?: string | null
  submitted_at: string
}

export interface ReviewSubmissionIntent {
  job_id: Id
  head_sha: string
  event: ReviewDecision
  body_hash: string
  comment_count: number
  created_at: string
}

export interface ReviewJobSummary {
  id: Id
  project_id: Id
  pr: number
  head_sha: string
  source: string
  reason: string
  status: ReviewJobStatus
  attempt_count: number
  max_attempts: number
  first_retryable_failure_at?: string | null
  next_attempt_at?: string | null
  reviewer_agent_id?: Id | null
  active_run_id?: Id | null
  failure?: ReviewFailure | null
  environment_warning?: ReviewEnvironmentWarning | null
  skip_reason?: ReviewSkipReason | null
  submission_intent?: ReviewSubmissionIntent | null
  submission_receipt?: ReviewSubmissionReceipt | null
  created_at: string
  updated_at: string
  finished_at?: string | null
}

export interface ReviewJobDetail extends ReviewJobSummary {
  attempts: ReviewRunSummary[]
}

export interface PullRequestReviewStatusSummary {
  active: number
  succeeded: number
  skipped: number
  failed: number
}

export type ProjectReviewDiscoveryState = "disabled" | "idle" | "scanning" | "partial" | "backoff"

export interface ProjectReviewDiscoverySnapshot {
  state: ProjectReviewDiscoveryState
  last_started_at?: string | null
  last_completed_at?: string | null
  next_scan_at?: string | null
  scanned: number
  eligible: number
  queued: number
  deduped: number
  watched: number
  closed: number
  draft: number
  already_reviewed: number
  suppressed: number
  errors: number
  last_error?: string | null
}

export interface PullRequestReviewSummary {
  pr: number
  latest_job: ReviewJobSummary
  history_count: number
  lifecycle_state: "open" | "merged" | "closed"
  state_changed_at?: string | null
}

export interface PullRequestStateRefreshSummary {
  checked: number
  newly_merged: number
  newly_closed: number
}

export interface PullRequestReviewPage {
  reviews: PullRequestReviewSummary[]
  page: number
  page_size: number
  total_items: number
  total_pages: number
  summary: PullRequestReviewStatusSummary
}

export interface PullRequestReviewHistoryItem {
  job: ReviewJobSummary
}

export interface PullRequestReviewHistoryPage {
  items: PullRequestReviewHistoryItem[]
  page: number
  page_size: number
  total_items: number
  total_pages: number
}

export interface ReviewRunDetail extends ReviewRunSummary {
  history?: ThreadTurnHistory | null
}

export interface ReviewRunsResponse {
  runs: ReviewRunSummary[]
}

export interface ProjectDetail extends ProjectSummary {
  maintainer_agent: AgentDetail
  agents: AgentSummary[]
  selected_agent_id: Id
  selected_agent: AgentDetail
  review_runs: ReviewRunSummary[]
  auth_status?: string
  mcp_status?: string
}

export interface TaskSummary {
  id: Id
  title: string
  status: string
  plan_status: string
  plan_version: number
  planner_agent_id?: Id
  current_agent_id?: Id | null
  agent_count: number
  review_rounds: number
  created_at: string
  updated_at: string
  last_error?: string | null
  final_report?: string | null
  [key: string]: unknown
}

export interface TaskDetail extends TaskSummary {
  plan: {
    status: string
    version: number
    title?: string | null
    markdown?: string | null
    revision_feedback?: string | null
    saved_at?: string | null
    approved_at?: string | null
  }
  plan_history: Array<{ version: number; saved_at?: string | null; revision_feedback?: string | null }>
  reviews: Array<{ id: string; round: number; passed: boolean; findings: string; summary: string; created_at: string }>
  agents: AgentSummary[]
  selected_agent_id: Id
  selected_agent: AgentDetail
  artifacts: Array<{ id: string; name: string; size_bytes: number; created_at: string }>
}

export interface ProviderModel {
  slug: string
  display_name: string
  description?: string | null
  context_window?: number | null
  max_context_window?: number | null
  max_output_tokens?: number | null
  parameters?: Array<{ name: string; label?: string | null; candidates: string[]; wire?: Record<string, unknown> }>
  transport: {
    protocol: string
    supported_connection_modes: string[]
    default_connection_mode: string
  }
  [key: string]: unknown
}

export interface PlProviderConfig {
  preset?: string | null
  name: string
  base_url: string
  bearer_token?: string | null
  bearer_token_env?: string | null
  http_headers?: Record<string, string> | null
  tool_wire_policy?: string
  apply_patch_tool_type?: string | null
  capabilities: Record<string, unknown>
  catalog: Record<string, unknown>
  [key: string]: unknown
}

export interface ProviderInstance {
  id: string
  config: PlProviderConfig
  models: ProviderModel[]
  has_api_key: boolean
  has_http_headers: boolean
}

export interface ProviderPreset {
  id: string
  display_name: string
  description?: string | null
  base_url: string
  suggested_model: string
  model_catalog_id: string
  icon_key?: string | null
  credential: { label: string; env_var?: string | null }
  transport: {
    protocol: string
    connection_modes: Array<{ id: string; display_name: string }>
    default_connection_mode: string
  }
  [key: string]: unknown
}

export interface ProviderCatalog {
  schema_version: number
  revision: string
  presets: ProviderPreset[]
  model_catalogs: Record<string, { models: ProviderModel[]; [key: string]: unknown }>
}

export interface ProvidersResponse {
  providers: ProviderInstance[]
}

export interface ProviderTestResponse {
  ok: boolean
  provider_id: string
  provider_name: string
  transport: ProviderModel["transport"]
  model: string
  base_url: string
  latency_ms: number
  output_preview: string
  usage?: TokenUsage | null
  error?: string | null
}

export interface ProductEventEnvelope {
  sequence: number
  timestamp: string
  type: string
  [key: string]: unknown
}

export interface AgentModelPreference {
  provider: string
  model: string
  effort?: string | null
}

export interface AgentConfigResponse {
  planner?: AgentModelPreference | null
  explorer?: AgentModelPreference | null
  executor?: AgentModelPreference | null
  reviewer?: AgentModelPreference | null
  effective_planner?: AgentModelPreference & { provider_name: string; model_name?: string | null }
  effective_explorer?: AgentModelPreference & { provider_name: string; model_name?: string | null }
  effective_executor?: AgentModelPreference & { provider_name: string; model_name?: string | null }
  effective_reviewer?: AgentModelPreference & { provider_name: string; model_name?: string | null }
  validation_error?: string | null
}

export interface SkillSummary {
  name: string
  description: string
  path: string
  scope: string
  enabled: boolean
  [key: string]: unknown
}

export interface SkillsResponse {
  roots: string[]
  skills: SkillSummary[]
  errors: Array<{ path: string; message: string }>
}

export interface GitAccountSummary {
  id: string
  provider: string
  label: string
  login?: string | null
  token_kind: string
  scopes: string[]
  status: string
  is_default: boolean
  has_token: boolean
  last_error?: string | null
  installation_id?: number | null
  installation_account?: string | null
}

export interface GitAccountsResponse {
  accounts: GitAccountSummary[]
  default_account_id?: string | null
}

export interface WebSearchSettingsResponse {
  config: {
    mode: string
    context_size?: string | null
    allowed_domains: string[]
    location?: { country?: string | null; region?: string | null; city?: string | null; timezone?: string | null } | null
  }
  roles: Record<string, {
    configured_mode: string
    effective_mode: string
    availability: string
    path?: string | null
    provider_id?: string | null
    model?: string | null
  }>
}

export interface McpServerAggregate {
  descriptor: { id: string; source: string; transport: string; endpoint: string; builtIn: boolean }
  enabled: boolean
  availability: string
  ready_agents: number
  failed_agents: number
  checking_agents: number
  total_agents: number
  tool_count: number
  config?: {
    scope: string
    enabled: boolean
    required: boolean
    command?: string | null
    args: string[]
    env_keys: string[]
    cwd?: string | null
    url?: string | null
    header_names: string[]
    bearer_token_env?: string | null
    has_bearer_token: boolean
    startup_timeout_secs?: number | null
    tool_timeout_secs?: number | null
    enabled_tools?: string[] | null
    disabled_tools: string[]
  } | null
}

export interface McpServersResponse { servers: McpServerAggregate[] }

export interface RelayStatus {
  enabled: boolean
  connected: boolean
  relay_url?: string | null
  node_id?: string | null
  message?: string | null
}

export interface RelaySettings {
  enabled: boolean
  url: string
  has_token: boolean
  node_id: string
}

export interface GithubAppSettings {
  app_id?: string | null
  base_url: string
  public_url?: string | null
  has_private_key: boolean
  app_slug?: string | null
  github_name?: string | null
  app_html_url?: string | null
  owner_login?: string | null
  owner_type?: string | null
  bot_login?: string | null
  bot_user_id?: number | null
  install_url?: string | null
  manage_url?: string | null
}

export interface GithubInstallationSummary {
  id: number
  account_login: string
  account_type: string
  repository_selection?: string | null
  events: string[]
}

export interface GithubRepositorySummary {
  id: number
  owner: string
  name: string
  full_name: string
  private: boolean
  clone_url: string
  html_url: string
  default_branch?: string | null
}

export interface RepositoryPackageSummary {
  name: string
  image: string
  tag: string
  html_url: string
}

export interface RuntimeDefaults { default_docker_image: string }
