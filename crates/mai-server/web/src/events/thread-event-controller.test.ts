import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

import { ThreadEventController } from "@/events/thread-event-controller"
import type { ThreadSubscriptionUpdate } from "@/events/thread-events.generated"
import { ThreadStoreRegistry } from "@/events/thread-store"

type EventListenerCallback = (event: MessageEvent<string>) => void

class FakeEventSource {
  static readonly CONNECTING = 0
  static readonly OPEN = 1
  static readonly CLOSED = 2
  static instances: FakeEventSource[] = []

  readonly url: string
  readyState = FakeEventSource.CONNECTING
  onerror: ((event: Event) => void) | null = null
  closed = false
  private readonly listeners = new Map<string, EventListenerCallback>()

  constructor(url: string | URL) {
    this.url = String(url)
    FakeEventSource.instances.push(this)
  }

  addEventListener(type: string, listener: EventListenerOrEventListenerObject | null) {
    if (typeof listener === "function") this.listeners.set(type, listener as EventListenerCallback)
  }

  close() {
    this.closed = true
    this.readyState = FakeEventSource.CLOSED
  }

  emit(type: "snapshot" | "notification", update: ThreadSubscriptionUpdate) {
    this.listeners.get(type)?.(new MessageEvent(type, { data: JSON.stringify(update) }))
  }

  disconnectPermanently() {
    this.readyState = FakeEventSource.CLOSED
    this.onerror?.(new Event("error"))
  }
}

describe("ThreadEventController", () => {
  let controllers: ThreadEventController[]

  beforeEach(() => {
    controllers = []
    FakeEventSource.instances = []
    vi.stubGlobal("EventSource", FakeEventSource)
  })

  afterEach(() => {
    for (const controller of controllers) controller.dispose()
    vi.useRealTimers()
    vi.unstubAllGlobals()
  })

  it("为两个 Thread 建立独立连接并拒绝跨 Thread envelope", () => {
    vi.useFakeTimers()
    const registry = new ThreadStoreRegistry()
    const a = registry.get("thread-a")
    const b = registry.get("thread-b")
    const controllerA = new ThreadEventController(a)
    const controllerB = new ThreadEventController(b)
    controllers.push(controllerA, controllerB)
    controllerA.connect()
    controllerB.connect()

    const sourceA = FakeEventSource.instances[0]
    const sourceB = FakeEventSource.instances[1]
    sourceA.emit("snapshot", { type: "snapshot", snapshot: snapshot("thread-a") })
    sourceB.emit("snapshot", { type: "snapshot", snapshot: snapshot("thread-b") })
    sourceA.emit("notification", {
      type: "notification",
      notification: {
        threadId: "thread-b",
        revision: 1,
        emittedAt: 2,
        notification: { type: "turnStarted", turn: turn("thread-b") },
      },
    })

    expect(a.getState().snapshot).toBeNull()
    expect(a.getState().connection).toBe("resyncing")
    expect(b.getState().snapshot?.thread.id).toBe("thread-b")
    expect(sourceA.closed).toBe(true)
    expect(sourceB.closed).toBe(false)
    vi.runAllTimers()
    expect(FakeEventSource.instances[2].url).toBe("/threads/thread-a/events")
  })

  it("底层连接永久关闭时创建新的 EventSource generation", () => {
    vi.useFakeTimers()
    const store = new ThreadStoreRegistry().get("thread-a")
    const controller = new ThreadEventController(store)
    controllers.push(controller)
    controller.connect()
    const source = FakeEventSource.instances[0]

    source.disconnectPermanently()

    expect(source.closed).toBe(true)
    expect(store.getState().connection).toBe("resyncing")
    expect(store.getState().connectionMessage).toBe("Thread stream disconnected")
    vi.runAllTimers()
    expect(FakeEventSource.instances[1].url).toBe("/threads/thread-a/events")
  })

  it("永久关闭使用有界退避并在 authoritative snapshot 后重置", () => {
    vi.useFakeTimers()
    const store = new ThreadStoreRegistry().get("thread-a")
    const controller = new ThreadEventController(store)
    controllers.push(controller)
    controller.connect()

    FakeEventSource.instances[0].disconnectPermanently()
    vi.advanceTimersByTime(499)
    expect(FakeEventSource.instances).toHaveLength(1)
    vi.advanceTimersByTime(1)
    expect(FakeEventSource.instances).toHaveLength(2)

    FakeEventSource.instances[1].emit("snapshot", {
      type: "snapshot",
      snapshot: snapshot("thread-a"),
    })
    FakeEventSource.instances[1].disconnectPermanently()
    vi.advanceTimersByTime(499)
    expect(FakeEventSource.instances).toHaveLength(2)
    vi.advanceTimersByTime(1)

    for (const delay of [1_000, 2_000, 4_000, 8_000, 10_000]) {
      FakeEventSource.instances.at(-1)?.disconnectPermanently()
      vi.advanceTimersByTime(delay)
    }
    FakeEventSource.instances.at(-1)?.disconnectPermanently()
    vi.runAllTimers()

    expect(FakeEventSource.instances).toHaveLength(8)
    expect(store.getState().connection).toBe("error")
    expect(store.getState().connectionMessage).toBe("Thread stream unavailable after repeated reconnect attempts")
  })

  it("Lagged 不进入 reducer，而是使 generation 失效", () => {
    vi.useFakeTimers()
    const store = new ThreadStoreRegistry().get("thread-a")
    const controller = new ThreadEventController(store)
    controllers.push(controller)
    controller.connect()
    const source = FakeEventSource.instances[0]
    source.emit("snapshot", { type: "snapshot", snapshot: snapshot("thread-a") })
    const generation = store.getState().generation
    source.emit("notification", {
      type: "notification",
      notification: {
        threadId: "thread-a",
        revision: 1,
        emittedAt: 2,
        notification: { type: "lagged", dropped: 7 },
      },
    })
    expect(store.getState().generation).toBe(generation + 1)
    expect(store.getState().snapshot).toBeNull()
    expect(store.getState().connectionMessage).toContain("lagged by 7")
  })

  it("dispose 会取消已经排队的重新订阅", () => {
    vi.useFakeTimers()
    const store = new ThreadStoreRegistry().get("thread-a")
    const controller = new ThreadEventController(store)
    controller.connect()
    FakeEventSource.instances[0].emit("notification", {
      type: "notification",
      notification: {
        threadId: "thread-a",
        revision: 1,
        emittedAt: 2,
        notification: { type: "turnStarted", turn: turn("thread-a") },
      },
    })

    controller.dispose()
    vi.runAllTimers()

    expect(FakeEventSource.instances).toHaveLength(1)
    expect(store.getState().connection).toBe("closed")
  })
})

function snapshot(threadId: string) {
  return {
    schemaVersion: 1,
    revision: 0,
    thread: {
      id: threadId,
      projectId: "",
      title: threadId,
      mode: "simple" as const,
      rootThreadId: threadId,
      role: "planner",
      agentPath: "root",
      status: "idle" as const,
      createdAt: 1,
      updatedAt: 1,
      archived: false,
    },
    items: [],
    interactions: [],
  }
}

function turn(threadId: string) {
  return {
    id: `${threadId}:turn`,
    threadId,
    state: { status: "queued" as const },
    startedAt: null,
    updatedAt: 2,
    completedAt: null,
  }
}
