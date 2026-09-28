import type { ThreadSubscriptionUpdate } from "@/events/thread-events.generated"
import type { ThreadStore } from "@/events/thread-store"

const reconnectDelays = [500, 1_000, 2_000, 4_000, 8_000, 10_000] as const

/**
 * 一条 Thread 的 SSE 订阅控制器。
 *
 * 权威首帧（`snapshot` 事件）替换整个产品状态；其后每一帧（`notification` 事件）都是
 * typed 通知，按新水位拼接到快照上。`lagged` 帧与任何投影错误都不进入 reducer，而是让
 * 当前 generation 失效并重新订阅，回到权威首帧。
 */
export class ThreadEventController {
  private source: EventSource | null = null
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
        this.store.getState().apply(generation, update.notification)
    }
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
