// @generated from pl-protocol Thread JSON contract. Do not edit by hand.

export type ThreadId = string

/** Thread 所选择的 Mode ID；wire 形式为 `mode.<name>`，内置为 `mode.simple` / `mode.task`。 */
export type ThreadModeId = string

export type ThreadWorkspaceMode = "local" | "worktree"

export interface Thread {
  id: ThreadId
  projectId: string
  title: string
  mode: ThreadModeId
  workspaceMode: ThreadWorkspaceMode
  workspacePath: string
  rootThreadId: ThreadId
  parentThreadId?: ThreadId
  role: string
  agentPath: string
  status: ThreadStatus
  createdAt: number
  updatedAt: number
  archived: boolean
}

export type ThreadStatus =
  | "idle"
  | "queued"
  | "running"
  | "waitingTool"
  | "waitingInteraction"
  | "cancelling"
  | "closing"
  | "closed"
  | "faulted"

export interface Turn {
  inputId?: string
  id: string
  threadId: ThreadId
  revision: number
  state: TurnState
  updatedAt: number
}

export type TurnState =
  | { kind: "queued"; data: { queuedAt: number } }
  | { kind: "running"; data: { startedAt: number; phase: TurnPhase } }
  | { kind: "completed"; data: { startedAt: number | null; completedAt: number; completion: TurnCompletion } }
  | { kind: "cancelled"; data: { startedAt: number | null; requestedAt: number; completedAt: number; cause: TurnCancellationCause } }
  | { kind: "failed"; data: { startedAt: number | null; completedAt: number; failure: TurnFailure } }
  | { kind: "budgetLimited"; data: { startedAt: number | null; completedAt: number; limit: BudgetLimitSnapshot; rollover: TurnRolloverOutcome } }

export type TurnPhase = "preparing" | "thinking" | "responding" | "planning" | "runningTool" | "persisting"

export type TurnCompletion = "normal" | "interactionRequested"

export type TurnCancellationCause =
  | { kind: "unspecified" }
  | { kind: "userRequested" }
  | { kind: "runtimeShutdown" }
  | { kind: "agentClosed" }
  | { kind: "interrupted" }
  | { kind: "recovery" }
  | { kind: "coalesced"; data: { targetTurnId: string } }

export type TurnRolloverOutcome =
  | { kind: "notAttempted" }
  | { kind: "succeeded" }
  | { kind: "failed"; data: { error: string } }

export interface BudgetLimitSnapshot {
  kind: "modelStep" | "toolCall" | "wait" | "wallClock" | "agentCount" | "agentDepth" | "finalization"
  usage: { modelSteps: number; toolCalls: number; waitCalls: number; elapsedMs: number }
}

export interface TurnFailure {
  category: "provider" | "providerCapacity" | "tool" | "validation" | "protocol" | "internal"
  providerKind?: "authentication" | "authorization" | "capacity" | "configuration" | "transport" | "protocol" | "unknown"
  code?: string
  httpStatus?: number
  message: string
  retry: { kind: "retryable"; retryAfterMs?: number } | { kind: "permanent" }
}

export type AttachmentModality = "image" | "video" | "file"

export interface ThreadAttachment {
  id: string
  modality: AttachmentModality
  mediaType: string
  filename?: string
  width?: number
  height?: number
  byteSize: number
}

export type ThreadTextChannel = "user" | "parentAgent" | "commentary" | "final"

export type ThreadContentLifecycle =
  | { kind: "streaming"; data: null }
  | { kind: "completed"; data: { completedAt: number } }
  | { kind: "failed"; data: { failedAt: number; error: string } }
  | { kind: "cancelled"; data: { cancelledAt: number; reason: string } }

export interface ThreadToolInvocation {
  toolCallId: string
  callId?: string
  providerItemId?: string
  name: string
  arguments?: string
  workingDirectory?: string
  taskId?: string
}

export interface ThreadToolOutput {
  result: string
  attachments?: ThreadAttachment[]
  outputArtifacts?: unknown[]
  exitCode?: number
}

export interface ThreadToolFailure {
  kind: "execution" | "timedOut" | "budgetLimited"
  message: string
}

export type ThreadToolState =
  | { kind: "queued"; data: null }
  | { kind: "cancelling"; data: { streamedOutput: string } }
  | { kind: "interrupted"; data: { interruptedAt: number; reason: string } }
  | { kind: "started"; data: null }
  | { kind: "streaming"; data: null }
  | { kind: "awaitingApproval"; data: null }
  | { kind: "approved"; data: null }
  | { kind: "running"; data: { streamedOutput?: string } }
  | { kind: "succeeded"; data: { completedAt: number; output: ThreadToolOutput } }
  | { kind: "failed"; data: { failedAt: number; failure: ThreadToolFailure; output?: ThreadToolOutput } }
  | { kind: "denied"; data: { deniedAt: number; reason: string } }
  | { kind: "cancelled"; data: { cancelledAt: number; reason: string } }

export type ThreadAgentState =
  | { kind: "queued"; data: null }
  | { kind: "running"; data: null }
  | { kind: "succeeded"; data: { completedAt: number; summary: string } }
  | { kind: "denied"; data: { deniedAt: number; reason: string } }
  | { kind: "cancelled"; data: { cancelledAt: number; reason: string } }
  | { kind: "failed"; data: { failedAt: number; error: string } }

export type ThreadInferenceState =
  | { kind: "running"; data: null }
  | { kind: "completed"; data: { completedAt: number; usage: TokenUsageSnapshot } }
  | { kind: "failed"; data: { failedAt: number; error: string } }
  | { kind: "cancelled"; data: { cancelledAt: number; reason: string } }

export interface SkillActivation {
  name: string
  source: string
  providerId: string
  resourceBase: { kind: "directory"; path: string } | { kind: "url"; url: string } | { kind: "opaque"; description: string }
  turnId: string
  cause: { kind: "tool"; toolCallId: string } | { kind: "userGesture"; invocationId: string }
  activatedAt: number
}

export interface ThreadRawPayload {
  format: string
  version: number
  content: string
}

export type ThreadItemState =
  | { kind: "raw"; data: { payloads: ThreadRawPayload[]; notice: string; recordedAt: number } }
  | { kind: "text"; data: { channel: ThreadTextChannel; text: string; attachments?: ThreadAttachment[]; lifecycle: ThreadContentLifecycle } }
  | { kind: "thinking"; data: { summary?: string[]; content?: string[]; lifecycle: ThreadContentLifecycle } }
  | { kind: "tool"; data: { invocation: ThreadToolInvocation; state: ThreadToolState } }
  | { kind: "agent"; data: { identity: { id: string; path: string; parentPath?: string; role: string; task: string; depth: number }; state: ThreadAgentState } }
  | { kind: "turn"; data: { state: TurnState; inputId?: string } }
  | { kind: "inference"; data: { inferenceId: string; model: string; state: ThreadInferenceState } }
  | { kind: "skill"; data: { activation: SkillActivation } }
  | { kind: "file"; data: { path: string; mediaType?: string; completedAt: number } }
  | { kind: "contextCompaction"; data: { beforeTokens: number | null; afterTokens: number | null; compactedAt: number } }

export interface ThreadItem {
  id: string
  threadId: ThreadId
  turnId: string
  ordinal: number
  revision: number
  createdAt: number
  updatedAt: number
  state: ThreadItemState
}

export interface TokenUsageSnapshot {
  promptTokens: number
  completionTokens: number
  cachedPromptTokens: number
  cacheWriteTokens: number
  cacheMissTokens: number
  reasoningTokens: number
  inferenceCount: number
  totalTokens: number
}

export interface CacheUsageSummary {
  inputTokens: number
  cacheReadTokens: number
  hitRate?: number
  hasIncompleteUsage: boolean
}

export interface ThreadModelRouteSnapshot {
  providerId: string
  model: string
  effort?: string
  revision: number
  available: boolean
  unavailableReason?: string
}

export interface ThreadRuntimeUsage {
  hasIncompleteUsage: boolean
  model: string
  contextWindow?: number
  latestContextTokens: number
  promptTokens: number
  completionTokens: number
  cachedPromptTokens: number
  cacheWriteTokens: number
  reasoningTokens: number
  inferenceCount: number
  totalTokens: number
  cacheUsage: CacheUsageSummary
  estimatedCosts?: RuntimeCostAmount[]
  estimatedCacheSavings?: RuntimeCostAmount[]
  hasUnpricedUsage: boolean
  promptGeneration?: number
  promptCachePolicy?: string
  prefixChangedReason?: PromptPrefixChangedReason
  updatedAt: number
}

export interface RuntimeCostAmount {
  currency: string
  amount: number
}

export type PromptPrefixChangedReason =
  | "initial"
  | "promptScopeChanged"
  | "providerChanged"
  | "modelChanged"
  | "baseInstructionsChanged"
  | "globalInstructionsChanged"
  | "modeRoleChanged"
  | "skillCatalogChanged"
  | "workspaceInstructionsChanged"
  | "requestPropertiesChanged"
  | "fixedPrefixChanged"
  | "toolSchemaChanged"
  | "contextCompacted"
  | "contextAppended"
  | "contextRecovered"

export interface TodoListSnapshot {
  callId: string
  agentId?: string
  path?: string
  parentPath?: string
  explanation?: string
  items: { step: string; status: "pending" | "inProgress" | "completed" }[]
}

export interface McpServerDescriptor {
  id: string
  source: string
  transport: string
  endpoint: string
  builtIn: boolean
}

export interface McpHealthSnapshot {
  generation: number
  servers: {
    server: McpServerDescriptor
    availability: string
    message: string | null
    lastCheckedAt: number | null
    toolCount: number | null
  }[]
}

export interface WorkflowRuntimeRunSnapshot {
  lineageId: string
  runId: string
  modeId: ThreadModeId
  graphRevision: number
  graphHash: string
  lifecycle: "active" | "terminal"
  currentStateId: string
  startedAt: number
  updatedAt: number
}

export interface WorkflowRuntimeSnapshot {
  revision: number
  currentRun?: WorkflowRuntimeRunSnapshot
}

export interface ThreadRuntimeSnapshot {
  threadId: ThreadId
  modelRoute?: ThreadModelRouteSnapshot
  usage: ThreadRuntimeUsage
  turnCompletionTokens: number
  turnDecodeMillis: number
  todo?: TodoListSnapshot
  activeSkills: string[]
  activeMcpServers: string[]
  activeLspServers: string[]
  progress?: string
  mcpHealth?: McpHealthSnapshot
  workflow?: WorkflowRuntimeSnapshot
  updatedAt: number
}

export type ThreadActivityKind =
  | "preparing"
  | "waitingApi"
  | "thinking"
  | "responding"
  | "planning"
  | "runningTool"
  | "awaitingApproval"
  | "awaitingInput"
  | "stopping"

export type ThreadActivityArguments = "commandLine" | "opaque" | "streaming" | "unavailable"

export type ThreadActivityToolState = "running" | "awaitingApproval" | "cancelling" | "finished"

export interface ThreadActivityToolEntry {
  callId: string
  taskId?: string
  name: string
  summary: string
  arguments: ThreadActivityArguments
  state: ThreadActivityToolState
  ordinal?: number
  startedAt?: number
}

export interface ThreadActivityTools {
  count: number
  background?: number
  active: ThreadActivityToolEntry[]
  latestStarted?: ThreadActivityToolEntry
}

export interface ThreadActivity {
  threadId: ThreadId
  identity: string
  revision: number
  turnId: string
  inputId?: string
  attemptId?: string
  kind: ThreadActivityKind
  summary: string
  summaryTruncated: boolean
  tools: ThreadActivityTools
}

export type HistoryFault =
  | "queueFull"
  | "writeFailed"
  | "writerUnavailable"
  | "noProgress"
  | "checkpointFailed"
  | "blobFailed"

export type ThreadStorageExecution = "running" | "pausing" | "paused"

export interface ThreadStorageState {
  fault?: HistoryFault
  faultGeneration: number
  acceptedSequence?: number
  durableSequence?: number
  execution: ThreadStorageExecution
  pressurePaused: boolean
  resumeRequired: boolean
  canResume: boolean
  lastError?: string
}

export interface ThreadSnapshot {
  schemaVersion: number
  revision: number
  thread: Thread
  activeTurn?: Turn
  interactions: InteractionRequest[]
  runtime?: ThreadRuntimeSnapshot
  activity?: ThreadActivity
  storage?: ThreadStorageState
}

export interface AgentSessionPlanConfirmationPurpose {
  expectedRevision: number
  operationId: string
  argumentHash: string
  planHash: string
}

export type InteractionPurpose =
  | { kind: "general" }
  | { kind: "agentSessionPlanConfirmation"; data: AgentSessionPlanConfirmationPurpose }

type PendingInteractionState = { kind: "pending"; data: { operationId: string } }
type CancelledInteractionState = { kind: "cancelled"; data: { operationId: string; cancelledAt: number; reason: string } }
type ExpiredInteractionState = { kind: "expired"; data: { operationId: string; expiredAt: number } }
type ResolvedUserInputState = { kind: "resolved"; data: { operationId: string; resolvedAt: number; answers: Record<string, { answers: string[] }> } }
type ResolvedToolApprovalState = { kind: "resolved"; data: { operationId: string; resolvedAt: number; decision: "approved" | "denied"; reason: string | null } }

export type InteractionContent =
  | { kind: "userInput"; data: { questions: UserQuestion[]; state: PendingInteractionState | ResolvedUserInputState | CancelledInteractionState | ExpiredInteractionState } }
  | { kind: "toolApproval"; data: { request: { name: string; arguments: unknown; workingDirectory: string | null; parentAgentId: string | null }; state: PendingInteractionState | ResolvedToolApprovalState | CancelledInteractionState | ExpiredInteractionState } }

export interface InteractionScope {
  threadId: ThreadId
  turnId: string
  itemId?: string
  toolId?: string
  agentPath?: string
  purpose: InteractionPurpose
}

export interface InteractionRequest {
  interactionId: string
  scope: InteractionScope
  revision: number
  content: InteractionContent
  continuation?: unknown
  createdAt: number
  updatedAt: number
}

export interface UserQuestion {
  id: string
  header: string
  question: string
  isOther: boolean
  isSecret: boolean
  options?: { label: string; description: string }[]
}

export type ThreadNotification =
  | { type: "turnStarted"; turn: Turn }
  | { type: "turnUpdated"; turn: Turn }
  | { type: "turnCompleted"; turn: Turn }
  | { type: "interactionChanged"; interaction: InteractionRequest }
  | { type: "threadRuntimeUpdated"; runtime: ThreadRuntimeSnapshot }
  | { type: "activityChanged"; activity: ThreadActivity | null }
  | { type: "storageChanged"; storage: ThreadStorageState | null }
  | { type: "lagged"; dropped: number }

export interface ThreadNotificationEnvelope {
  threadId: ThreadId
  epoch: number
  baseRevision: number
  revision: number
  emittedAt: number
  notification: ThreadNotification
}

export type ThreadSubscriptionUpdate =
  | { type: "snapshot"; snapshot: ThreadSnapshot }
  | { type: "notification"; notification: ThreadNotificationEnvelope }

export type ThreadContextDisposition = "active" | "rolledBack"

export interface ThreadTurnHistory {
  turn: Turn
  items: ThreadItem[]
  contextDisposition: ThreadContextDisposition
}

export interface ThreadTurnPage {
  turns: ThreadTurnHistory[]
  nextCursor?: string
}
