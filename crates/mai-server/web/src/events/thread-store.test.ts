import { beforeEach, describe, expect, it } from "vitest"

import type { ThreadNotification, ThreadNotificationEnvelope, ThreadSnapshot } from "@/events/thread-events.generated"
import { ThreadStoreRegistry } from "@/events/thread-store"

describe("ThreadStoreRegistry", () => {
  let registry: ThreadStoreRegistry

  beforeEach(() => { registry = new ThreadStoreRegistry() })

  it("交错发布不会污染另一个 Thread", () => {
    const a = registry.get("thread-a")
    const b = registry.get("thread-b")
    const generationA = a.getState().begin()
    const generationB = b.getState().begin()
    a.getState().replace(generationA, snapshot("thread-a"))
    b.getState().replace(generationB, snapshot("thread-b"))

    a.getState().apply(generationA, envelope("thread-a", { type: "turnStarted", turn: turn("thread-a") }))
    b.getState().apply(generationB, envelope("thread-b", { type: "turnStarted", turn: turn("thread-b") }))

    expect(a.getState().snapshot?.activeTurn?.id).toBe("thread-a:turn")
    expect(b.getState().snapshot?.activeTurn?.id).toBe("thread-b:turn")
  })

  it("旧 generation 不能修改重新订阅后的状态", () => {
    const store = registry.get("thread-a")
    const oldGeneration = store.getState().begin()
    const currentGeneration = store.getState().begin()
    store.getState().replace(oldGeneration, snapshot("thread-a", "stale"))
    store.getState().replace(currentGeneration, snapshot("thread-a", "current"))
    expect(store.getState().snapshot?.thread.title).toBe("current")
  })

  it("广播 epoch 变化会拒绝迟到帧", () => {
    const store = registry.get("thread-a")
    const generation = store.getState().begin()
    store.getState().replace(generation, snapshot("thread-a"))
    store.getState().apply(generation, envelope("thread-a", { type: "turnStarted", turn: turn("thread-a") }, { epoch: 1 }))
    expect(() => store.getState().apply(generation, envelope("thread-a", { type: "turnStarted", turn: turn("thread-a") }, { epoch: 2, baseRevision: 1, revision: 2 }))).toThrow(/epoch changed/)
  })

  it("首帧到达前拒绝通知", () => {
    const store = registry.get("thread-a")
    const generation = store.getState().begin()
    expect(() => store.getState().apply(generation, envelope("thread-a", { type: "turnStarted", turn: turn("thread-a") }))).toThrow(/before authoritative snapshot/)
  })
})

function snapshot(threadId: string, title = threadId): ThreadSnapshot {
  return {
    schemaVersion: 14,
    revision: 0,
    thread: {
      id: threadId,
      projectId: "",
      title,
      mode: "mode.simple",
      workspaceMode: "local",
      workspacePath: "",
      rootThreadId: threadId,
      role: "planner",
      agentPath: "root",
      status: "idle",
      createdAt: 1,
      updatedAt: 1,
      archived: false,
    },
    interactions: [],
  }
}

function turn(threadId: string) {
  return { id: `${threadId}:turn`, threadId, revision: 0, state: { kind: "queued" as const, data: { queuedAt: 2 } }, updatedAt: 2 }
}

function envelope(
  threadId: string,
  notification: ThreadNotification,
  overrides: { epoch?: number; baseRevision?: number; revision?: number } = {},
): ThreadNotificationEnvelope {
  const revision = overrides.revision ?? 1
  return {
    threadId,
    epoch: overrides.epoch ?? 1,
    baseRevision: overrides.baseRevision ?? 0,
    revision,
    emittedAt: revision,
    notification,
  }
}
