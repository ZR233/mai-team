import { describe, expect, it } from "vitest"

import type { ThreadItem, ThreadNotificationEnvelope, ThreadSnapshot } from "@/events/thread-events.generated"
import { applyThreadNotification, ThreadProjectionError, validateThreadSnapshot } from "@/events/thread-reducer"

describe("Thread reducer", () => {
  it("拒绝跨 Thread envelope", () => {
    expect(() => applyThreadNotification(snapshot("thread-a"), notification("thread-b", 1, {
      type: "turnStarted",
      turn: turn("thread-b"),
    }))).toThrow(ThreadProjectionError)
  })

  it("拒绝 Thread revision gap", () => {
    expect(() => applyThreadNotification(snapshot("thread-a"), notification("thread-a", 2, {
      type: "turnStarted",
      turn: turn("thread-a"),
    }))).toThrow(/revision gap/)
  })

  it("拒绝 Item revision gap 与跨 Turn 更新", () => {
    const initial = snapshot("thread-a", [item("thread-a", "turn-a", 1)])
    expect(() => applyThreadNotification(initial, notification("thread-a", 1, {
      type: "itemDelta",
      delta: { itemId: "item-1", revision: 3, field: "text", delta: "late" },
    }))).toThrow(/Item item-1 revision gap/)
    expect(() => applyThreadNotification(initial, notification("thread-a", 1, {
      type: "itemCompleted",
      item: { ...item("thread-a", "turn-b", 1), revision: 2 },
    }))).toThrow(/crossed Thread or Turn ownership/)
  })

  it("Lagged 使 projection 失效而不是普通应用", () => {
    expect(() => applyThreadNotification(snapshot("thread-a"), notification("thread-a", 1, {
      type: "lagged",
      dropped: 4,
    }))).toThrow(/lagged by 4/)
  })

  it("只接受目标 Thread 的 authoritative snapshot", () => {
    expect(validateThreadSnapshot("thread-a", snapshot("thread-a")).thread.id).toBe("thread-a")
    expect(() => validateThreadSnapshot("thread-a", snapshot("thread-b"))).toThrow(/snapshot mismatch/)
  })
})

function snapshot(threadId: string, items: ThreadItem[] = []): ThreadSnapshot {
  return {
    schemaVersion: 1,
    revision: 0,
    thread: { id: threadId, projectId: "", title: threadId, mode: "simple", rootThreadId: threadId, role: "planner", agentPath: "root", status: "idle", createdAt: 1, updatedAt: 1, archived: false },
    items,
    interactions: [],
  }
}

function item(threadId: string, turnId: string, revision: number): ThreadItem {
  return { id: "item-1", threadId, turnId, ordinal: 0, revision, status: "streaming", createdAt: 1, updatedAt: 1, content: { type: "agentMessage", channel: "commentary", text: "" } }
}

function turn(threadId: string) {
  return { id: `${threadId}:turn`, threadId, state: { status: "queued" as const }, startedAt: null, updatedAt: 1, completedAt: null }
}

function notification(threadId: string, revision: number, payload: ThreadNotificationEnvelope["notification"]): ThreadNotificationEnvelope {
  return { threadId, revision, emittedAt: revision, notification: payload }
}
