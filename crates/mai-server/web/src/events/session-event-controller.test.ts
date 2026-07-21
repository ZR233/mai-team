import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

import { SessionEventController } from "@/events/session-event-controller"
import { emptySession } from "@/events/session-reducer"
import {
  sessionMessage,
  sessionSnapshot,
  snapshotFrame,
  textPart,
  transientEvent,
} from "@/events/session-test-fixtures"
import type { SessionStreamFrame } from "@/events/session-events.generated"
import { useSessionStore } from "@/events/session-store"

type EventListenerCallback = (event: MessageEvent<string>) => void

class FakeEventSource {
  static readonly CONNECTING = 0
  static readonly OPEN = 1
  static readonly CLOSED = 2
  static instances: FakeEventSource[] = []

  readonly url: string
  readyState = FakeEventSource.CONNECTING
  onopen: ((event: Event) => void) | null = null
  onerror: ((event: Event) => void) | null = null
  closed = false
  private listeners = new Map<string, EventListenerCallback>()

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

  emit(type: string, frame: SessionStreamFrame) {
    this.listeners.get(type)?.(new MessageEvent(type, { data: JSON.stringify(frame) }))
  }
}

describe("session event controller", () => {
  let controller: SessionEventController

  beforeEach(() => {
    FakeEventSource.instances = []
    vi.stubGlobal("EventSource", FakeEventSource)
    useSessionStore.setState({
      generation: 0,
      connection: "idle",
      connectionMessage: null,
      view: emptySession(),
    })
    controller = new SessionEventController()
  })

  afterEach(() => {
    controller.dispose()
    vi.useRealTimers()
    vi.unstubAllGlobals()
  })

  it("closes session A before subscribing to B and rejects late A frames", () => {
    controller.connect("session-a")
    const sourceA = FakeEventSource.instances[0]
    sourceA.emit("snapshot", snapshotFrame(sessionSnapshot("session-a")))

    controller.connect("session-b")
    const sourceB = FakeEventSource.instances[1]
    sourceB.emit("snapshot", snapshotFrame(sessionSnapshot("session-b")))
    sourceA.emit("event", {
      type: "event",
      event: {
        eventId: "late-a",
        sessionId: "session-a",
        emittedAt: 1,
        position: { persistence: "durable", sequence: 1 },
        kind: { type: "errorOccurred", message: "late", severity: "recoverable" },
      },
    })

    expect(sourceA.closed).toBe(true)
    expect(useSessionStore.getState().view.sessionId).toBe("session-b")
    expect(useSessionStore.getState().view.lastError).toBeNull()
    expect(FakeEventSource.instances.filter((source) => !source.closed)).toEqual([sourceB])
  })

  it("reconnects without a cursor after resync is required", () => {
    vi.useFakeTimers()
    controller.connect("session-a")
    const source = FakeEventSource.instances[0]
    source.emit("snapshot", snapshotFrame(sessionSnapshot("session-a", 8)))
    source.emit("resyncRequired", {
      type: "resyncRequired",
      reason: { type: "revisionGap", partId: "part-1", expected: 2, actual: 3 },
    })
    vi.runAllTimers()

    const replacement = FakeEventSource.instances[1]
    expect(source.closed).toBe(true)
    expect(replacement.url).toBe("/sessions/session-a/events")
  })

  it("batches transient deltas until the next animation frame", () => {
    let pendingFrame: FrameRequestCallback | null = null
    vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
      pendingFrame = callback
      return 1
    })
    vi.stubGlobal("cancelAnimationFrame", () => {
      pendingFrame = null
    })
    const snapshot = sessionSnapshot()
    snapshot.messages.push(sessionMessage())
    snapshot.parts.push(textPart())
    controller.connect("session-a")
    const source = FakeEventSource.instances[0]
    source.emit("snapshot", snapshotFrame(snapshot))
    source.emit("event", {
      type: "event",
      event: transientEvent({
        type: "partDelta",
        delta: { partId: "part-1", revision: 1, field: "text", delta: " world" },
      }, 1),
    })

    expect(useSessionStore.getState().view.parts["part-1"]?.revision).toBe(0)
    if (pendingFrame) (pendingFrame as FrameRequestCallback)(16)
    expect(useSessionStore.getState().view.parts["part-1"]?.content).toEqual({
      type: "text",
      channel: "final",
      text: "Hello world",
    })
  })
})
