/**
 * 时间轴投影：ThreadItem[] → TimelineEntry[]。
 *
 * 核心规则（Codex 风格）：同一 turn 内连续的工具调用合并为一个
 * toolGroup 条目；任何可见的非工具条目（文字、思考、计划、技能激活）
 * 或 turn 边界都会打断分组。连续段长度为 1 时退化为单个 tool 条目。
 *
 * key 从条目 id 派生且流式更新期间稳定，保证分组折叠状态不被 React 重置。
 */

import type { ThreadItem } from "@/events/thread-events.generated"

import { formatDuration } from "./duration"
import { buildToolActivity, isFailedOutcome, type ToolActivity, type ToolCallItem } from "./tool-activity"

export type TimelineEntry =
  | { kind: "item"; key: string; item: ThreadItem }
  | { kind: "tool"; key: string; activity: ToolActivity }
  | { kind: "toolGroup"; key: string; group: ToolActivityGroup }
  | { kind: "reasoningGroup"; key: string; group: ReasoningActivityGroup }

export interface ToolActivityGroup {
  activities: ToolActivity[]
  /** "Used 4 tools" / "Using 4 tools"。 */
  countLabel: string
  /** 分组整体时长；仍有工具在运行时为 null。 */
  durationLabel: string | null
  active: boolean
  failedCount: number
}

export type ReasoningItem = ThreadItem & { state: Extract<ThreadItem["state"], { kind: "thinking" }> }

export interface ReasoningActivityGroup {
  items: ReasoningItem[]
  latestSummary: string | null
  durationLabel: string | null
  active: boolean
}

export function buildTimelineEntries(items: ThreadItem[]): TimelineEntry[] {
  const entries: TimelineEntry[] = []
  let pendingTools: ToolCallItem[] = []
  let pendingReasoning: ReasoningItem[] = []

  const flushTools = () => {
    if (pendingTools.length === 0) return
    const [first] = pendingTools
    if (pendingTools.length === 1) {
      entries.push({ kind: "tool", key: first.id, activity: buildToolActivity(first) })
    } else {
      entries.push({ kind: "toolGroup", key: `tool-group:${first.id}`, group: buildToolActivityGroup(pendingTools) })
    }
    pendingTools = []
  }

  const flushReasoning = () => {
    if (pendingReasoning.length === 0) return
    const [first] = pendingReasoning
    entries.push({
      kind: "reasoningGroup",
      key: `reasoning-group:${first.id}`,
      group: buildReasoningActivityGroup(pendingReasoning),
    })
    pendingReasoning = []
  }

  const ordered = [...items].sort((left, right) => left.ordinal - right.ordinal || left.id.localeCompare(right.id))
  for (const item of ordered) {
    if (!isVisibleItem(item)) continue
    const currentTurnId = pendingTools[0]?.turnId
    // 只有与当前待分组段同 turn 的 toolCall 才能继续并入。
    if (isToolCallItem(item) && (currentTurnId === undefined || item.turnId === currentTurnId)) {
      flushReasoning()
      pendingTools.push(item)
      continue
    }
    flushTools()
    if (isToolCallItem(item)) {
      flushReasoning()
      pendingTools.push(item)
    } else if (isReasoningItem(item)) {
      const reasoningTurnId = pendingReasoning[0]?.turnId
      if (reasoningTurnId !== undefined && item.turnId !== reasoningTurnId) flushReasoning()
      pendingReasoning.push(item)
    } else {
      flushReasoning()
      entries.push({ kind: "item", key: item.id, item })
    }
  }
  flushTools()
  flushReasoning()

  return entries
}

function isToolCallItem(item: ThreadItem): item is ToolCallItem {
  return item.state.kind === "tool"
}

function isReasoningItem(item: ThreadItem): item is ReasoningItem {
  return item.state.kind === "thinking"
}

function isVisibleItem(item: ThreadItem): boolean {
  switch (item.state.kind) {
    case "agent":
    case "turn":
    case "inference":
    case "file":
    case "contextCompaction":
      return false
    case "thinking":
      return [...(item.state.data.summary ?? []), ...(item.state.data.content ?? [])].some((part) => part.trim().length > 0)
    case "text":
    case "plan":
    case "skill":
    case "tool":
      return true
  }
}

function buildReasoningActivityGroup(items: ReasoningItem[]): ReasoningActivityGroup {
  const active = items.some((item) => !isTerminalItem(item))
  const startedAt = Math.min(...items.map((item) => item.createdAt))
  const endedAt = Math.max(...items.map((item) => item.updatedAt))
  return {
    items: [...items],
    latestSummary: [...items]
      .reverse()
      .flatMap((item) => [...(item.state.data.summary ?? [])].reverse())
      .map((summary) => summary.trim())
      .find(Boolean) ?? null,
    durationLabel: active ? null : formatDuration(startedAt, endedAt),
    active,
  }
}

function isTerminalItem(item: ReasoningItem): boolean {
  return item.state.data.lifecycle.kind !== "streaming"
}

function buildToolActivityGroup(items: ToolCallItem[]): ToolActivityGroup {
  const activities = items.map(buildToolActivity)
  const active = activities.some((activity) => activity.outcome === "active")
  const failedCount = activities.filter((activity) => isFailedOutcome(activity.outcome)).length
  const count = activities.length
  const countLabel = `${active ? "Using" : "Used"} ${count} tool${count === 1 ? "" : "s"}`

  return {
    activities,
    countLabel,
    durationLabel: active ? null : groupDuration(items),
    active,
    failedCount,
  }
}

function groupDuration(items: ToolCallItem[]): string | null {
  const startedAt = Math.min(...items.map((item) => item.createdAt))
  const endedAt = Math.max(...items.map((item) => item.updatedAt))
  return formatDuration(startedAt, endedAt)
}
