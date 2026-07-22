import type { AgentMessage, ReviewRunDetail } from "@/api/product-types"
import type { SessionPart } from "@/events/session-events.generated"
import { parseToolText, type ToolPresentationInput } from "@/features/session/tool-presentation"

export type ReviewActivityItem =
  | { kind: "tool"; id: string; tool: ToolPresentationInput }
  | { kind: "message"; id: string; role: AgentMessage["role"]; content: string }
  | {
      kind: "conclusion"
      id: string
      outcome?: string
      reviewEvent?: string
      summary?: string
      error?: string
    }

interface ToolEvent {
  part: SessionPart
  eventIndex: number
}

export function buildReviewActivity(detail: ReviewRunDetail): ReviewActivityItem[] {
  const tools = latestToolEvents(detail)
  const usedPartIds = new Set<string>()
  const activity: ReviewActivityItem[] = []

  detail.messages.forEach((message, index) => {
    const content = message.content.trim()
    if (!content || message.role === "system") return

    if (message.role === "tool") {
      const contentKey = toolContentKey(content)
      const matching = tools.find(({ part }) => !usedPartIds.has(part.partId) && toolContentKey(toolResult(part)) === contentKey)
      if (matching) usedPartIds.add(matching.part.partId)
      activity.push({
        kind: "tool",
        id: matching ? `part:${matching.part.partId}` : `message:${message.created_at}:${index}`,
        tool: matching ? presentationInput(matching.part, content) : { result: content },
      })
      return
    }

    const conclusion = parseConclusion(content)
    if (message.role === "assistant" && conclusion) {
      activity.push({ kind: "conclusion", id: `conclusion:${message.created_at}:${index}`, ...conclusion })
      return
    }

    activity.push({ kind: "message", id: `message:${message.created_at}:${index}`, role: message.role, content })
  })

  for (const { part } of tools) {
    if (usedPartIds.has(part.partId)) continue
    activity.push({ kind: "tool", id: `part:${part.partId}`, tool: presentationInput(part) })
  }

  return activity
}

function latestToolEvents(detail: ReviewRunDetail): ToolEvent[] {
  const latest = new Map<string, ToolEvent>()
  detail.events.forEach((event, eventIndex) => {
    if (event.kind.type !== "partChanged" || event.kind.part.content.type !== "tool") return
    const part = event.kind.part
    const current = latest.get(part.partId)
    if (!current || part.revision > current.part.revision || (part.revision === current.part.revision && eventIndex > current.eventIndex)) {
      latest.set(part.partId, { part, eventIndex })
    }
  })
  return [...latest.values()].sort((left, right) => left.eventIndex - right.eventIndex)
}

function presentationInput(part: SessionPart, resultOverride?: string): ToolPresentationInput {
  if (part.content.type !== "tool") return {}
  return {
    ...part.content.tool,
    result: resultOverride ?? part.content.tool.result,
    status: part.status,
    error: part.error,
  }
}

function toolResult(part: SessionPart): string {
  return part.content.type === "tool" ? part.content.tool.result?.trim() ?? "" : ""
}

function toolContentKey(content: string): string {
  const parsed = parseToolText(content)
  if (!parsed.structured) return parsed.text
  try {
    return JSON.stringify(parsed.value)
  } catch {
    return parsed.text
  }
}

function parseConclusion(content: string) {
  const parsed = parseToolText(content).value
  if (!isRecord(parsed) || typeof parsed.outcome !== "string") return null
  if (!("review_event" in parsed || "summary" in parsed || "error" in parsed)) return null
  return {
    outcome: stringValue(parsed.outcome),
    reviewEvent: stringValue(parsed.review_event),
    summary: stringValue(parsed.summary),
    error: stringValue(parsed.error),
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

function stringValue(value: unknown): string | undefined {
  return typeof value === "string" && value ? value : undefined
}
