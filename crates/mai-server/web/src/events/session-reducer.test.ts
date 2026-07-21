import { describe, expect, it } from "vitest"

import { applySessionEvent, normalizeSnapshot } from "@/events/session-reducer"
import {
  durableEvent,
  sessionMessage,
  sessionSnapshot,
  textPart,
  transientEvent,
} from "@/events/session-test-fixtures"
import type { SessionEventEnvelope } from "@/events/session-events.generated"

describe("session reducer", () => {
  it("normalizes a snapshot and applies ordered durable facts", () => {
    const snapshot = sessionSnapshot()
    snapshot.messages.push(sessionMessage())
    snapshot.parts.push(textPart())
    let view = normalizeSnapshot(snapshot)

    view = applySessionEvent(view, durableEvent({
      type: "timelineEventAppended",
      event: {
        eventId: "todo-1",
        sessionId: "session-a",
        sequence: 1,
        createdAt: 1,
        kind: {
          type: "todoListChanged",
          snapshot: {
            callId: "todo-call",
            items: [{ step: "Inspect events", status: "inProgress" }],
          },
        },
      },
    }, 1))
    view = applySessionEvent(view, durableEvent({
      type: "runtimeChanged",
      runtime: {
        sessionId: "session-a",
        usage: {
          model: "future-model",
          latestContextTokens: 120,
          promptTokens: 100,
          completionTokens: 20,
          cachedPromptTokens: 40,
          totalTokens: 120,
          estimatedCosts: [],
          hasUnpricedUsage: false,
          updatedAt: 2,
        },
        activeSkills: ["review"],
        activeMcpServers: [],
        activeLspServers: [],
        agentCount: 1,
        updatedAt: 2,
      },
    }, 2))

    expect(view.throughSequence).toBe(2)
    expect(view.timelineEvents[0]?.kind).toEqual({
      type: "todoListChanged",
      snapshot: {
        callId: "todo-call",
        items: [{ step: "Inspect events", status: "inProgress" }],
      },
    })
    expect(view.runtime?.usage.model).toBe("future-model")
  })

  it("applies transient deltas without advancing the durable cursor", () => {
    const snapshot = sessionSnapshot()
    snapshot.messages.push(sessionMessage())
    snapshot.parts.push(textPart())
    const current = normalizeSnapshot(snapshot)

    const next = applySessionEvent(current, transientEvent({
      type: "partDelta",
      delta: { partId: "part-1", revision: 1, field: "text", delta: " world" },
    }, 1))

    expect(next.parts["part-1"]?.content).toEqual({
      type: "text",
      channel: "final",
      text: "Hello world",
    })
    expect(next.throughSequence).toBe(0)
  })

  it("rejects durable and transient gaps", () => {
    const snapshot = sessionSnapshot()
    snapshot.messages.push(sessionMessage())
    snapshot.parts.push(textPart())
    const view = normalizeSnapshot(snapshot)

    expect(() => applySessionEvent(view, durableEvent({
      type: "errorOccurred",
      message: "gap",
      severity: "recoverable",
    }, 2))).toThrow("session sequence gap")
    expect(() => applySessionEvent(view, transientEvent({
      type: "partDelta",
      delta: { partId: "part-1", revision: 2, field: "text", delta: "gap" },
    }, 2))).toThrow("revision gap")
  })

  it("keeps runtime skills and agent count synchronized with canonical facts", () => {
    let view = normalizeSnapshot(sessionSnapshot())
    view = applySessionEvent(view, durableEvent({
      type: "runtimeChanged",
      runtime: {
        sessionId: "session-a",
        usage: {
          model: "future-model",
          latestContextTokens: 0,
          promptTokens: 0,
          completionTokens: 0,
          cachedPromptTokens: 0,
          totalTokens: 0,
          estimatedCosts: [],
          hasUnpricedUsage: false,
          updatedAt: 1,
        },
        activeSkills: [],
        activeMcpServers: ["search"],
        activeLspServers: [],
        agentCount: 0,
        updatedAt: 1,
      },
    }, 1))
    view = applySessionEvent(view, durableEvent({
      type: "skillActivated",
      activation: {
        name: "review",
        source: "project",
        path: "/project/repo/skills/review/SKILL.md",
        turnId: "turn-1",
        toolCallId: "skill-call",
        activatedAt: 2,
      },
    }, 2))
    view = applySessionEvent(view, durableEvent({
      type: "agentChanged",
      agent: {
        id: "child",
        sessionId: "session-a",
        path: "/root/child",
        parentPath: "/root",
        role: "explorer",
        task: "inspect",
        status: "running",
        depth: 1,
        updatedAt: 3,
      },
    }, 3))

    expect(view.runtime?.activeSkills).toEqual(["review"])
    expect(view.runtime?.activeMcpServers).toEqual(["search"])
    expect(view.runtime?.agentCount).toBe(1)
  })

  it("ignores an event from another session and tolerates a future event kind", () => {
    const current = normalizeSnapshot(sessionSnapshot())
    const other = applySessionEvent(current, durableEvent({
      type: "errorOccurred",
      message: "other",
      severity: "transient",
    }, 1, "session-b"))
    expect(other).toBe(current)

    const future = durableEvent({ type: "futureEvent" } as unknown as SessionEventEnvelope["kind"], 1)
    expect(applySessionEvent(current, future).throughSequence).toBe(1)
  })
})
