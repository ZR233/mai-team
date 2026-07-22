import { describe, expect, it } from "vitest"

import type { ReviewRunDetail } from "@/api/product-types"
import type { SessionEventEnvelope, SessionPart } from "@/events/session-events.generated"

import { buildReviewActivity } from "./review-activity"

describe("review activity", () => {
  it("keeps the highest tool revision, matches archived results, and recognizes the conclusion", () => {
    const detail = reviewDetail({
      messages: [
        message("assistant", "I am checking the changed files.", 1),
        message("assistant", "", 2),
        message("system", "hidden prompt", 3),
        message("tool", '{ "status": "completed", "exitCode": 0, "stdout": "ok", "stderr": "" }', 4),
        message("assistant", '{"outcome":"review_submitted","review_event":"approve","summary":"No blocking findings."}', 5),
      ],
      events: [toolEvent(toolPart(0, "old command", "old result"), 1), toolEvent(toolPart(2, "cargo test", '{"status":"completed","exitCode":0,"stdout":"ok","stderr":""}'), 2)],
    })

    expect(buildReviewActivity(detail)).toEqual([
      expect.objectContaining({ kind: "message", role: "assistant", content: "I am checking the changed files." }),
      expect.objectContaining({ kind: "tool", id: "part:tool-part", tool: expect.objectContaining({ name: "exec", arguments: '{"command":"cargo test"}' }) }),
      expect.objectContaining({ kind: "conclusion", outcome: "review_submitted", reviewEvent: "approve", summary: "No blocking findings." }),
    ])
  })

  it("infers unmatched historical tool results and appends event-only tools once", () => {
    const detail = reviewDetail({
      messages: [message("tool", '{"revision":3,"totalLines":8,"text":"# Notes"}', 1)],
      events: [toolEvent(toolPart(1, "cargo check", '{"status":"completed","exitCode":0,"stdout":"done","stderr":""}'), 1)],
    })

    const activity = buildReviewActivity(detail)
    expect(activity).toHaveLength(2)
    expect(activity[0]).toMatchObject({ kind: "tool", tool: { result: expect.stringContaining('"revision":3') } })
    expect(activity[1]).toMatchObject({ kind: "tool", id: "part:tool-part", tool: { name: "exec" } })
  })
})

function reviewDetail(value: Pick<ReviewRunDetail, "messages" | "events">): ReviewRunDetail {
  return {
    id: "review-1",
    status: "completed",
    pr: 7,
    started_at: "2026-01-01T00:00:00Z",
    ...value,
  }
}

function message(role: "assistant" | "system" | "tool", content: string, order: number) {
  return { role, content, created_at: `2026-01-01T00:00:0${order}Z` }
}

function toolPart(revision: number, command: string, result: string): SessionPart {
  return {
    partId: "tool-part",
    messageId: "message-1",
    sessionId: "session-1",
    turnId: "turn-1",
    order: 0,
    revision,
    status: "completed",
    createdAt: 1,
    updatedAt: revision + 1,
    content: { type: "tool", tool: { toolCallId: "call-1", name: "exec", arguments: JSON.stringify({ command }), result } },
  }
}

function toolEvent(part: SessionPart, sequence: number): SessionEventEnvelope {
  return {
    eventId: `event-${sequence}`,
    sessionId: "session-1",
    emittedAt: sequence,
    position: { persistence: "durable", sequence },
    kind: { type: "partChanged", part },
  }
}
