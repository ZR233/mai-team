import { beforeEach, describe, expect, it } from "vitest"

import type { ThreadSnapshot } from "@/events/thread-events.generated"
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

    a.getState().apply(generationA, { threadId: "thread-a", revision: 1, emittedAt: 2, notification: { type: "turnStarted", turn: { id: "turn-a", threadId: "thread-a", state: { status: "queued" }, startedAt: null, updatedAt: 2, completedAt: null } } })
    b.getState().apply(generationB, { threadId: "thread-b", revision: 1, emittedAt: 2, notification: { type: "turnStarted", turn: { id: "turn-b", threadId: "thread-b", state: { status: "queued" }, startedAt: null, updatedAt: 2, completedAt: null } } })

    expect(a.getState().snapshot?.activeTurn?.id).toBe("turn-a")
    expect(b.getState().snapshot?.activeTurn?.id).toBe("turn-b")
  })

  it("旧 generation 不能修改重新订阅后的状态", () => {
    const store = registry.get("thread-a")
    const oldGeneration = store.getState().begin()
    const currentGeneration = store.getState().begin()
    store.getState().replace(oldGeneration, snapshot("thread-a", "stale"))
    store.getState().replace(currentGeneration, snapshot("thread-a", "current"))
    expect(store.getState().snapshot?.thread.title).toBe("current")
  })
})

function snapshot(threadId: string, title = threadId): ThreadSnapshot {
  return {
    schemaVersion: 1,
    revision: 0,
    thread: { id: threadId, projectId: "", title, mode: "simple", rootThreadId: threadId, role: "planner", agentPath: "root", status: "idle", createdAt: 1, updatedAt: 1, archived: false },
    items: [],
    interactions: [],
  }
}
