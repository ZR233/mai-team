// @generated from pl-protocol. Do not edit by hand.

export type SessionEventPosition =
  | { persistence: "durable"; sequence: number }
  | { persistence: "transient"; revision: number }

export type SessionStreamFrame =
  | { type: "snapshot"; snapshot: SessionViewSnapshot }
  | { type: "event"; event: SessionEventEnvelope }
  | { type: "resyncRequired"; reason: SessionResyncReason }

export type SessionResyncReason =
  | { type: "lagged"; events: number }
  | { type: "cursorExpired"; requested: number; oldestAvailable: number }
  | { type: "replayLimitExceeded"; available: number; limit: number }
  | { type: "revisionGap"; partId: string; expected: number; actual: number }
  | { type: "projectionInvariant"; message: string }

export interface SessionEventEnvelope {
  eventId: string
  sessionId: string
  sourceAgentId?: string
  turnId?: string
  emittedAt: number
  position: SessionEventPosition
  kind: SessionEventKind
}

export type SessionEventKind =
  | { type: "turnChanged"; turn: SessionTurn }
  | { type: "messageChanged"; message: SessionMessage }
  | { type: "messageRemoved"; messageId: string }
  | { type: "partChanged"; part: SessionPart }
  | { type: "partRemoved"; messageId: string; partId: string }
  | { type: "partDelta"; delta: SessionPartDelta }
  | { type: "interactionChanged"; event: InteractionChangedEvent }
  | { type: "agentChanged"; agent: SessionAgentSnapshot }
  | { type: "timelineEventAppended"; event: SessionTimelineEvent }
  | { type: "runtimeChanged"; runtime: SessionRuntimeSnapshot }
  | { type: "skillActivated"; activation: SkillActivation }
  | { type: "planChanged"; event: PlanLifecycleEvent }
  | { type: "contextCompacted"; compaction: SessionContextCompaction }
  | { type: "errorOccurred"; message: string; severity: string }

export interface SessionViewSnapshot {
  schemaVersion: number
  sessionId: string
  throughSequence: number
  turn?: SessionTurn
  messages: SessionMessage[]
  parts: SessionPart[]
  interactions: InteractionRequest[]
  agents: SessionAgentSnapshot[]
  timelineEvents: SessionTimelineEvent[]
  runtime?: SessionRuntimeSnapshot
  activatedSkills: SkillActivation[]
  planEvents: PlanLifecycleEvent[]
}

export interface SessionTurn {
  turnId: string
  sessionId: string
  status: "queued" | "contextLoading" | "waitingForModel" | "streaming" | "waitingForInteraction" | "runningTool" | "persisting" | "completed" | "failed" | "cancelled"
  reason?: string
  updatedAt: number
}

export interface SessionMessage {
  messageId: string
  sessionId: string
  turnId: string
  role: "user" | "assistant" | "system"
  status: "queued" | "streaming" | "completed" | "failed" | "cancelled"
  createdAt: number
  updatedAt: number
  completedAt?: number
  error?: string
  metadata?: Record<string, unknown>
}

export type SessionPartContent =
  | { type: "text"; channel: "user" | "commentary" | "final"; text?: string; attachments?: SessionAttachment[] }
  | { type: "reasoning"; text?: string }
  | { type: "tool"; tool: SessionToolPart }
  | { type: "agent"; agent: SessionAgentPart }
  | { type: "turn" }
  | { type: "inference"; inferenceId: string; model: string }
  | { type: "plan"; content?: string }
  | { type: "file"; path: string; mediaType?: string }

export interface SessionPart {
  partId: string
  messageId: string
  sessionId: string
  turnId: string
  order: number
  revision: number
  status: "started" | "streaming" | "awaitingApproval" | "approved" | "denied" | "running" | "completed" | "failed" | "interrupted" | "budgetLimited"
  createdAt: number
  updatedAt: number
  completedAt?: number
  error?: string
  content: SessionPartContent
  usage?: TokenUsageSnapshot
  synthetic?: boolean
  ignored?: boolean
}

export interface SessionPartDelta {
  partId: string
  revision: number
  field: "text" | "reasoning.summary" | "planContent" | "tool.arguments" | "tool.result"
  delta: string
  chunkIndex?: number
}

export interface SessionAttachment {
  id: string
  mediaType: string
  filename?: string
  width?: number
  height?: number
  byteSize: number
  dataUrl?: string
}

export interface SessionToolPart {
  toolCallId: string
  callId?: string
  providerItemId?: string
  name: string
  arguments?: string
  result?: string
  outputArtifacts?: Record<string, unknown>[]
  exitCode?: number
  timedOut?: boolean
  workingDirectory?: string
  denialReason?: string
  activityGroupId?: string
}

export interface SessionAgentPart {
  id: string
  path: string
  parentPath?: string
  role: string
  task: string
  status: AgentStatus
  summary?: string
  depth: number
  error?: string
  reason?: string
}

export interface SessionAgentSnapshot extends SessionAgentPart {
  sessionId: string
  budgetLimitKind?: "modelStep" | "toolCall" | "wait" | "wallClock" | "agentCount" | "agentDepth" | "finalization"
  budgetUsage?: { modelSteps: number; toolCalls: number; waitCalls: number; elapsedMs: number }
  runtimeUsage?: SessionRuntimeUsage
  updatedAt: number
}

export interface SessionTimelineEvent {
  eventId: string
  sessionId: string
  sequence: number
  createdAt: number
  kind: SessionTimelineEventKind
}

export type AgentStatus = "queued" | "running" | "waiting" | "completed" | "errored" | "interrupted" | "shutdown" | "notFound"

export type SessionTimelineEventKind =
  | {
      type: "subAgentActivity"
      callId: string
      agentId?: string
      path?: string
      parentPath?: string
      kind: "spawned" | "messageQueued" | "followupStarted" | "waitCompleted" | "closed"
      status?: AgentStatus
      message?: string
      timedOut?: boolean
      error?: string
    }
  | { type: "todoListChanged"; snapshot: TodoListSnapshot }

export interface TodoListSnapshot {
  callId: string
  agentId?: string
  path?: string
  parentPath?: string
  explanation?: string
  items: Array<{ step: string; status: "pending" | "inProgress" | "completed" }>
}

export interface InteractionChangedEvent {
  interaction: InteractionRequest
}

export interface InteractionRequest {
  interactionId: string
  kind: "userInput" | "toolApproval" | "planConfirmation"
  status: "pending" | "resolved" | "cancelled" | "expired"
  scope: {
    sessionId: string
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

export type InteractionResolution =
  | { type: "userInput"; answers: Record<string, { answers: string[] }> }
  | { type: "toolApproval"; decision: "approved" | "denied"; reason?: string }
  | { type: "planConfirmation"; decision: "implementFreshContext" | "continuePlanning" | "dismiss"; content?: string; reason?: string }

export interface UserQuestion {
  id: string
  header: string
  question: string
  isOther?: boolean
  isSecret?: boolean
  options?: Array<{ label: string; description: string }>
}

export interface SkillActivation {
  name: string
  source: string
  path: string
  turnId: string
  toolCallId: string
  activatedAt: number
}

export interface PlanLifecycleEvent {
  planId: string
  state: "pendingConfirmation" | "accepted" | "implementing" | "implemented" | "implementationFailed" | "continuedPlanning" | "dismissed" | "cancelled"
  turnId?: string
  reason?: string
  updatedAt: number
}

export interface SessionRuntimeSnapshot {
  sessionId: string
  usage: SessionRuntimeUsage
  activeSkills: string[]
  activeMcpServers: string[]
  activeLspServers: string[]
  agentCount: number
  mcpHealth?: Record<string, unknown>
  updatedAt: number
}

export interface SessionRuntimeUsage {
  model: string
  contextWindow?: number
  latestContextTokens: number
  promptTokens: number
  completionTokens: number
  cachedPromptTokens: number
  totalTokens: number
  cacheHitRate?: number
  estimatedCosts: Record<string, unknown>[]
  hasUnpricedUsage: boolean
  updatedAt: number
}

export interface SessionContextCompaction {
  beforeTokens: number
  afterTokens: number
  compactedAt: number
}

export interface TokenUsageSnapshot {
  promptTokens: number
  completionTokens: number
  cachedPromptTokens: number
  totalTokens: number
}
