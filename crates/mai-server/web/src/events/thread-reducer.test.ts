import { describe, expect, it } from "vitest"

import type { ThreadItem, ThreadNotificationEnvelope, ThreadSnapshot, Turn } from "@/events/thread-events.generated"
import { applyThreadNotification, ThreadProjectionError, validateThreadSnapshot } from "@/events/thread-reducer"

describe("Thread reducer PL v2 投影", () => {
  it("拒绝跨 Thread、revision gap 与跨 Turn 更新", () => {
    expect(() => applyThreadNotification(snapshot("thread-a"), notification("thread-b", 1, { type: "turnStarted", turn: turn("thread-b") }))).toThrow(ThreadProjectionError)
    expect(() => applyThreadNotification(snapshot("thread-a"), notification("thread-a", 2, { type: "turnStarted", turn: turn("thread-a") }))).toThrow(/revision gap/)

    const initial = snapshot("thread-a", [textItem("thread-a", "turn-a", 1)])
    expect(() => applyThreadNotification(initial, notification("thread-a", 1, {
      type: "itemCompleted",
      item: { ...textItem("thread-a", "turn-b", 1), revision: 2 },
    }))).toThrow(/crossed Thread or Turn ownership/)
  })

  it("按 canonical tagged delta 更新文字与指定思考 chunk", () => {
    const initial = snapshot("thread-a", [textItem("thread-a", "turn-a", 1)])
    const withText = applyThreadNotification(initial, notification("thread-a", 1, {
      type: "itemDelta",
      delta: { itemId: "item-1", revision: 2, delta: { kind: "text", data: { delta: "hello" } } },
    }))
    expect(withText.items[0]?.state).toEqual({ kind: "text", data: { channel: "commentary", text: "hello", lifecycle: { kind: "streaming", data: null } } })

    const thinking = snapshot("thread-a", [{ ...textItem("thread-a", "turn-a", 1), state: { kind: "thinking", data: { summary: ["first"], lifecycle: { kind: "streaming", data: null } } } }])
    const withChunk = applyThreadNotification(thinking, notification("thread-a", 1, {
      type: "itemDelta",
      delta: { itemId: "item-1", revision: 2, delta: { kind: "thinkingSummary", data: { chunkIndex: 1, delta: "second" } } },
    }))
    expect(withChunk.items[0]?.state).toEqual({ kind: "thinking", data: { summary: ["first", "second"], lifecycle: { kind: "streaming", data: null } } })
  })

  it("拒绝 Item revision gap 与跳过 reasoning chunk", () => {
    const initial = snapshot("thread-a", [textItem("thread-a", "turn-a", 1)])
    expect(() => applyThreadNotification(initial, notification("thread-a", 1, {
      type: "itemDelta",
      delta: { itemId: "item-1", revision: 3, delta: { kind: "text", data: { delta: "late" } } },
    }))).toThrow(/Item item-1 revision gap/)

    const thinking = snapshot("thread-a", [{ ...textItem("thread-a", "turn-a", 1), state: { kind: "thinking", data: { lifecycle: { kind: "streaming", data: null } } } }])
    expect(() => applyThreadNotification(thinking, notification("thread-a", 1, {
      type: "itemDelta",
      delta: { itemId: "item-1", revision: 2, delta: { kind: "thinkingContent", data: { chunkIndex: 2, delta: "skip" } } },
    }))).toThrow(/skipped reasoning chunk/)
  })

  it("Lagged 使 projection 失效，snapshot 必须属于目标 Thread", () => {
    expect(() => applyThreadNotification(snapshot("thread-a"), notification("thread-a", 1, { type: "lagged", dropped: 4 }))).toThrow(/lagged by 4/)
    expect(validateThreadSnapshot("thread-a", snapshot("thread-a")).thread.id).toBe("thread-a")
    expect(() => validateThreadSnapshot("thread-a", snapshot("thread-b"))).toThrow(/snapshot mismatch/)
  })
})

function snapshot(threadId: string, items: ThreadItem[] = []): ThreadSnapshot {
  return {
    schemaVersion: 7,
    revision: 0,
    thread: { id: threadId, projectId: "", title: threadId, mode: "simple", rootThreadId: threadId, role: "planner", agentPath: "root", status: "idle", createdAt: 1, updatedAt: 1, archived: false },
    items,
    interactions: [],
  }
}

function textItem(threadId: string, turnId: string, revision: number): ThreadItem {
  return { id: "item-1", threadId, turnId, ordinal: 0, revision, createdAt: 1, updatedAt: 1, state: { kind: "text", data: { channel: "commentary", text: "", lifecycle: { kind: "streaming", data: null } } } }
}

function turn(threadId: string): Turn {
  return { id: `${threadId}:turn`, threadId, revision: 0, state: { kind: "queued", data: { queuedAt: 1 } }, updatedAt: 1 }
}

function notification(threadId: string, revision: number, payload: ThreadNotificationEnvelope["notification"]): ThreadNotificationEnvelope {
  return { threadId, revision, emittedAt: revision, notification: payload }
}
