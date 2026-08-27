import type { ThreadSubscriptionUpdate } from "@/events/thread-events.generated"
import { isItemDelta, type ThreadStore } from "@/events/thread-store"

const reconnectDelays = [500, 1_000, 2_000, 4_000, 8_000, 10_000] as const

export class ThreadEventController {
  private source: EventSource | null = null
  private animationFrame: number | null = null
  private reconnectTimer: number | null = null
  private reconnectAttempt = 0

  constructor(private readonly store: ThreadStore) {}

  connect() {
    this.disconnect()
    const state = this.store.getState()
    const generation = state.begin()
    const source = new EventSource(`/threads/${encodeURIComponent(state.threadId)}/events`)
    this.source = source

    const consume = (message: MessageEvent<string>) => {
      if (generation !== this.store.getState().generation) return
      try {
        this.consume(generation, parseThreadSubscriptionUpdate(JSON.parse(message.data)))
      } catch (error) {
        this.resubscribe(generation, error instanceof Error ? error.message : "Invalid Thread update")
      }
    }
    source.addEventListener("snapshot", consume as EventListener)
    source.addEventListener("notification", consume as EventListener)
    source.onerror = () => {
      if (generation !== this.store.getState().generation) return
      const closed = source.readyState === EventSource.CLOSED
      if (closed) {
        this.resubscribe(generation, "Thread stream disconnected")
        return
      }
      this.store.getState().setConnection(
        generation,
        "connecting",
        "Reconnecting…",
      )
    }
  }

  disconnect() {
    this.source?.close()
    this.source = null
    if (this.animationFrame !== null) cancelAnimationFrame(this.animationFrame)
    this.animationFrame = null
    if (this.reconnectTimer !== null) window.clearTimeout(this.reconnectTimer)
    this.reconnectTimer = null
  }

  dispose() {
    this.disconnect()
    const state = this.store.getState()
    state.setConnection(state.generation, "closed")
  }

  private consume(generation: number, update: ThreadSubscriptionUpdate) {
    switch (update.type) {
      case "snapshot":
        this.store.getState().replace(generation, update.snapshot)
        this.reconnectAttempt = 0
        return
      case "notification":
        if (update.notification.notification.type === "lagged") {
          this.resubscribe(generation, `Thread stream lagged by ${update.notification.notification.dropped}`)
          return
        }
        if (isItemDelta(update.notification)) {
          this.store.getState().bufferDelta(generation, update.notification)
          this.scheduleFlush(generation)
        } else {
          this.flush(generation)
          this.store.getState().apply(generation, update.notification)
        }
    }
  }

  private scheduleFlush(generation: number) {
    if (this.animationFrame !== null) return
    this.animationFrame = requestAnimationFrame(() => {
      this.animationFrame = null
      try {
        this.flush(generation)
      } catch (error) {
        this.resubscribe(generation, error instanceof Error ? error.message : "Invalid Item delta")
      }
    })
  }

  private flush(generation: number) {
    this.store.getState().flushDeltas(generation)
  }

  private resubscribe(generation: number, message: string) {
    if (generation !== this.store.getState().generation) return
    const nextGeneration = this.store.getState().invalidate(generation, message)
    this.disconnect()
    if (this.reconnectAttempt >= reconnectDelays.length) {
      this.store.getState().setConnection(
        nextGeneration,
        "error",
        "Thread stream unavailable after repeated reconnect attempts",
      )
      return
    }
    const delay = reconnectDelays[this.reconnectAttempt]
    this.reconnectAttempt += 1
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = null
      this.connect()
    }, delay)
  }
}

function parseThreadSubscriptionUpdate(value: unknown): ThreadSubscriptionUpdate {
  if (!isRecord(value)) throw new Error("Thread update must be an object")
  switch (value.type) {
    case "snapshot":
      if (!isRecord(value.snapshot)) throw new Error("Thread snapshot payload is missing")
      return value as unknown as ThreadSubscriptionUpdate
    case "notification":
      if (!isRecord(value.notification)) throw new Error("Thread notification envelope is missing")
      return value as unknown as ThreadSubscriptionUpdate
    default:
      throw new Error("Unknown Thread subscription update")
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}
