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
  status: "idle" | "running" | "waiting" | "completed" | "failed" | "closed"
  createdAt: number
  updatedAt: number
  archived: boolean
}

export interface Turn {
  id: string
  threadId: ThreadId
  state: TurnState
  failure?: TurnFailure
  startedAt: number | null
  updatedAt: number
  completedAt: number | null
}

export type TurnState =
  | { status: "queued" }
  | { status: "inProgress"; phase: "preparing" | "thinking" | "responding" | "planning" | "runningTool" | "waitingInteraction" | "persisting" }
  | { status: "completed" }
  | { status: "failed"; reason: string }
  | { status: "interrupted"; reason: string }

export interface TurnFailure {
  category: "provider" | "providerCapacity" | "tool" | "validation" | "internal"
  providerKind?: "authentication" | "authorization" | "capacity" | "configuration" | "transport" | "protocol" | "unknown"
  code?: string
  httpStatus?: number
  message: string
  retry: { kind: "retryable"; retryAfterMs?: number } | { kind: "permanent" }
}

export interface ThreadToolCall {
  toolCallId: string
  callId?: string
  providerItemId?: string
  name: string
  arguments?: string
  result?: string
  outputArtifacts?: unknown[]
  exitCode?: number
  timedOut: boolean
  workingDirectory?: string
  denialReason?: string
}

export type ThreadItemContent =
  | { type: "userMessage"; text?: string; attachments?: ThreadAttachment[] }
  | { type: "agentMessage"; channel: "commentary" | "final"; text?: string }
  | { type: "reasoning"; summary?: string[]; content?: string[] }
  | { type: "plan"; content?: string }
  | { type: "toolCall"; tool: ThreadToolCall }
  | { type: "file"; path: string; mediaType?: string }
  | { type: "contextCompaction"; beforeTokens: number; afterTokens: number; compactedAt: number }

export interface ThreadAttachment {
  id: string
  mediaType: string
  filename?: string
  width?: number
  height?: number
  byteSize: number
  dataUrl?: string
}

export interface ThreadItem {
  id: string
  threadId: ThreadId
  turnId: string
  ordinal: number
  revision: number
  status: "started" | "streaming" | "awaitingApproval" | "approved" | "denied" | "running" | "completed" | "failed" | "interrupted" | "budgetLimited"
  createdAt: number
  updatedAt: number
  completedAt?: number
  error?: string
  content: ThreadItemContent
  usage?: TokenUsageSnapshot
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
  field: "text" | "reasoning.summary" | "reasoning.content" | "planContent" | "tool.arguments" | "tool.result"
  delta: string
  chunkIndex?: number
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
  mcpHealth?: Record<string, unknown>
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

export interface InteractionRequest {
  interactionId: string
  kind: "userInput" | "toolApproval" | "planConfirmation"
  status: "pending" | "resolved" | "cancelled" | "expired"
  scope: {
    threadId: ThreadId
    turnId: string
    itemId?: string
    toolId?: string
    agentPath?: string
  }
  payload: InteractionPayload
  createdAt: number
  updatedAt: number
  resolvedAt?: number
  resolution?: InteractionResolution
}

export type InteractionPayload =
  | { type: "userInput"; questions: UserQuestion[] }
  | { type: "toolApproval"; name: string; arguments: unknown; workingDirectory?: string; parentAgentId?: string }
  | { type: "planConfirmation"; planId: string; content: string }

export interface UserQuestion {
  id: string
  header: string
  question: string
  isOther: boolean
  isSecret: boolean
  options?: { label: string; description: string }[]
}

export type InteractionResolution =
  | { type: "userInput"; answers: Record<string, { answers: string[] }> }
  | { type: "toolApproval"; decision: "approved" | "denied"; reason?: string }
  | { type: "planConfirmation"; decision: "implementFreshContext" | "continuePlanning" | "dismiss"; content?: string; reason?: string }

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
