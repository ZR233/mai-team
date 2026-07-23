import { describe, expect, it } from "vitest"

import type { SessionPart } from "@/events/session-events.generated"

import { buildTimelineRenderItems } from "./timeline-presentation"

describe("timeline presentation", () => {
  it("groups only adjacent tools with the same explicit activity group", () => {
    const parts = [
      toolPart("tool-1", 0, "group-a"),
      toolPart("tool-2", 1, "group-a"),
      textPart("text-1", 2),
      toolPart("tool-3", 3, "group-a"),
      toolPart("tool-4", 4, "group-b"),
      toolPart("tool-5", 5, "group-b"),
      toolPart("tool-6", 6),
    ]

    const items = buildTimelineRenderItems(parts)

    expect(items.map((item) => ({
      kind: item.kind,
      key: item.key,
      parts: item.kind === "toolGroup" ? item.parts.map((part) => part.partId) : [item.part.partId],
    }))).toEqual([
      { kind: "toolGroup", key: "tool-group:group-a:tool-1", parts: ["tool-1", "tool-2"] },
      { kind: "part", key: "text-1", parts: ["text-1"] },
      { kind: "part", key: "tool-3", parts: ["tool-3"] },
      { kind: "toolGroup", key: "tool-group:group-b:tool-4", parts: ["tool-4", "tool-5"] },
      { kind: "part", key: "tool-6", parts: ["tool-6"] },
    ])
  })

  it("keeps a stable group key while consecutive tools are appended", () => {
    const first = toolPart("tool-1", 0, "turn:tools")
    const second = toolPart("tool-2", 1, "turn:tools")
    const third = toolPart("tool-3", 2, "turn:tools")

    const before = buildTimelineRenderItems([first, second])
    const after = buildTimelineRenderItems([first, second, third])

    expect(before[0]?.key).toBe("tool-group:turn:tools:tool-1")
    expect(after[0]?.key).toBe(before[0]?.key)
    expect(after[0]?.kind === "toolGroup" ? after[0].parts.map((part) => part.partId) : []).toEqual([
      "tool-1",
      "tool-2",
      "tool-3",
    ])
  })
})

function toolPart(partId: string, order: number, activityGroupId?: string): SessionPart {
  return {
    partId,
    messageId: "message",
    sessionId: "session",
    turnId: "turn",
    order,
    revision: 0,
    status: "completed",
    createdAt: order,
    updatedAt: order,
    completedAt: order,
    content: {
      type: "tool",
      tool: {
        toolCallId: `${partId}-call`,
        name: "read_file",
        activityGroupId,
      },
    },
  }
}

function textPart(partId: string, order: number): SessionPart {
  return {
    partId,
    messageId: "message",
    sessionId: "session",
    turnId: "turn",
    order,
    revision: 0,
    status: "completed",
    createdAt: order,
    updatedAt: order,
    completedAt: order,
    content: { type: "text", channel: "commentary", text: "Checking the result." },
  }
}
