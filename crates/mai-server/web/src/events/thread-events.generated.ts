// @generated from pl-protocol Thread JSON contract. Do not edit by hand.

export type ThreadId = string

export interface Thread {
  id: ThreadId
  projectId: string
  title: string
  mode: "simple" | "task"
  rootThreadId: ThreadId
  parentThreadId?: ThreadId
  role: string
  agentPath: string
  status: "idle" | "queued" | "running" | "waitingTool" | "waitingInteraction" | "cancelling" | "closing" | "closed" | "faulted"
  createdAt: number
  updatedAt: number
  archived: boolean
}

export interface Turn {
  id: string
  threadId: ThreadId
  revision: number
  state: TurnState
  updatedAt: number
}

export type TurnState =
  | { kind: "queued"; data: { queuedAt: number } }
  | { kind: "running"; data: { startedAt: number; phase: TurnPhase } }
  | { kind: "completed"; data: { startedAt: number | null; completedAt: number; completion: "normal" | "interactionRequested" } }
  | { kind: "cancelled"; data: { startedAt: number | null; requestedAt: number; completedAt: number; cause: TurnCancellationCause } }
  | { kind: "failed"; data: { startedAt: number | null; completedAt: number; failure: TurnFailure } }
  | { kind: "budgetLimited"; data: { startedAt: number | null; completedAt: number; limit: BudgetLimitSnapshot; rollover: TurnRolloverOutcome } }

export type TurnPhase = "preparing" | "thinking" | "responding" | "planning" | "runningTool" | "persisting"

export type TurnCancellationCause =
  | { kind: "userRequested" }
  | { kind: "runtimeShutdown" }
  | { kind: "agentClosed" }
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

export interface ThreadAttachment {
  id: string
  mediaType: string
  filename?: string
  width?: number
  height?: number
  byteSize: number
  dataUrl?: string
}

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
}

export interface ThreadToolOutput {
  result: string
  outputArtifacts?: unknown[]
  exitCode?: number
}

export interface ThreadToolFailure {
  kind: "execution" | "timedOut" | "budgetLimited"
  message: string
}

export type ThreadToolState =
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

export type ThreadItemState =
  | { kind: "text"; data: { channel: "user" | "commentary" | "final"; text: string; attachments?: ThreadAttachment[]; lifecycle: ThreadContentLifecycle } }
  | { kind: "thinking"; data: { summary?: string[]; content?: string[]; lifecycle: ThreadContentLifecycle } }
  | { kind: "tool"; data: { invocation: ThreadToolInvocation; state: ThreadToolState } }
  | { kind: "agent"; data: { identity: { id: string; path: string; parentPath?: string; role: string; task: string; depth: number }; state: ThreadAgentState } }
  | { kind: "turn"; data: { state: TurnState } }
  | { kind: "inference"; data: { inferenceId: string; model: string; state: ThreadInferenceState } }
  | { kind: "plan"; data: { content: string; lifecycle: ThreadContentLifecycle } }
  | { kind: "skill"; data: { activation: SkillActivation } }
  | { kind: "file"; data: { path: string; mediaType?: string; completedAt: number } }
  | { kind: "contextCompaction"; data: { beforeTokens: number; afterTokens: number; compactedAt: number } }

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

export interface ThreadItemDelta {
  itemId: string
  revision: number
  delta:
    | { kind: "text"; data: { delta: string } }
    | { kind: "thinkingSummary"; data: { chunkIndex: number; delta: string } }
    | { kind: "thinkingContent"; data: { chunkIndex: number; delta: string } }
    | { kind: "plan"; data: { delta: string } }
    | { kind: "toolArguments"; data: { delta: string } }
    | { kind: "toolResult"; data: { delta: string } }
}

export interface ThreadRuntimeUsage {
  model: string
  contextWindow?: number
  latestContextTokens: number
  promptTokens: number
  completionTokens: number
  cachedPromptTokens: number
  cacheWriteTokens: number
  cacheMissTokens: number
  reasoningTokens: number
  inferenceCount: number
  totalTokens: number
  cacheHitRate?: number
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

export interface ThreadRuntimeSnapshot {
  threadId: ThreadId
  usage: ThreadRuntimeUsage
  todo?: TodoListSnapshot
  activeSkills: string[]
  activeMcpServers: string[]
  activeLspServers: string[]
  progress?: string
  mcpHealth?: {
    generation: number
    servers: { server: { id: string; source: string; transport: string; endpoint: string; builtIn: boolean }; availability: string; message: string | null; lastCheckedAt: number | null; toolCount: number | null }[]
  }
  updatedAt: number
}

export interface ThreadSnapshot {
  schemaVersion: number
  revision: number
  thread: Thread
  activeTurn?: Turn
  items: ThreadItem[]
  interactions: InteractionRequest[]
  runtime?: ThreadRuntimeSnapshot
}

type PendingInteractionState = { kind: "pending"; data: { operationId: string } }
type CancelledInteractionState = { kind: "cancelled"; data: { operationId: string; cancelledAt: number; reason: string } }
type ExpiredInteractionState = { kind: "expired"; data: { operationId: string; expiredAt: number } }
type ResolvedUserInputState = { kind: "resolved"; data: { operationId: string; resolvedAt: number; answers: Record<string, { answers: string[] }> } }
type ResolvedToolApprovalState = { kind: "resolved"; data: { operationId: string; resolvedAt: number; decision: "approved" | "denied"; reason: string | null } }
type ResolvedPlanConfirmationState = { kind: "resolved"; data: { operationId: string; resolvedAt: number; decision: "confirm" | "revisePlan"; content: string | null; reason: string | null } }

export type InteractionContent =
  | { kind: "userInput"; data: { questions: UserQuestion[]; state: PendingInteractionState | ResolvedUserInputState | CancelledInteractionState | ExpiredInteractionState } }
  | { kind: "toolApproval"; data: { request: { name: string; arguments: unknown; workingDirectory: string | null; parentAgentId: string | null }; state: PendingInteractionState | ResolvedToolApprovalState | CancelledInteractionState | ExpiredInteractionState } }
  | { kind: "planConfirmation"; data: { planId: string; content: string; state: PendingInteractionState | ResolvedPlanConfirmationState | CancelledInteractionState | ExpiredInteractionState } }

export interface InteractionRequest {
  interactionId: string
  scope: { threadId: ThreadId; turnId: string; itemId?: string; toolId?: string; agentPath?: string }
  revision: number
  content: InteractionContent
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
  | { type: "itemStarted"; item: ThreadItem }
  | { type: "itemDelta"; delta: ThreadItemDelta }
  | { type: "itemCompleted"; item: ThreadItem }
  | { type: "interactionChanged"; interaction: InteractionRequest }
  | { type: "threadRuntimeUpdated"; runtime: ThreadRuntimeSnapshot }
  | { type: "lagged"; dropped: number }

export interface ThreadNotificationEnvelope {
  threadId: ThreadId
  revision: number
  emittedAt: number
  notification: ThreadNotification
}

export type ThreadSubscriptionUpdate =
  | { type: "snapshot"; snapshot: ThreadSnapshot }
  | { type: "notification"; notification: ThreadNotificationEnvelope }

export interface ThreadTurnHistory {
  turn: Turn
  items: ThreadItem[]
  contextDisposition: "active" | "rolledBack"
}

export interface ThreadTurnPage {
  turns: ThreadTurnHistory[]
  nextCursor?: string
}
