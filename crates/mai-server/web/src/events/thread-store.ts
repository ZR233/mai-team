import { createStore, type StoreApi } from "zustand/vanilla"

import type { ThreadItemDelta, ThreadNotificationEnvelope, ThreadSnapshot } from "@/events/thread-events.generated"
import { applyThreadNotification, validateThreadSnapshot } from "@/events/thread-reducer"

export type ThreadConnectionState = "idle" | "connecting" | "live" | "resyncing" | "error" | "closed"

export interface ThreadStoreState {
  threadId: string
  generation: number
  connection: ThreadConnectionState
  connectionMessage: string | null
  snapshot: ThreadSnapshot | null
  deltaBuffer: ThreadNotificationEnvelope[]
  begin(): number
  replace(generation: number, snapshot: ThreadSnapshot): void
  apply(generation: number, notification: ThreadNotificationEnvelope): void
  bufferDelta(generation: number, notification: ThreadNotificationEnvelope): void
  flushDeltas(generation: number): void
  setConnection(generation: number, connection: ThreadConnectionState, message?: string): void
  invalidate(generation: number, message: string): number
}

export type ThreadStore = StoreApi<ThreadStoreState>

export class ThreadStoreRegistry {
  private readonly stores = new Map<string, ThreadStore>()

  get(threadId: string): ThreadStore {
    const current = this.stores.get(threadId)
    if (current) return current
    const created = createThreadStore(threadId)
    this.stores.set(threadId, created)
    return created
  }

  delete(threadId: string) {
    this.stores.delete(threadId)
  }

  clear() {
    this.stores.clear()
  }
}

export const threadStores = new ThreadStoreRegistry()

function createThreadStore(threadId: string): ThreadStore {
  return createStore<ThreadStoreState>((set, get) => ({
    threadId,
    generation: 0,
    connection: "idle",
    connectionMessage: null,
    snapshot: null,
    deltaBuffer: [],
    begin() {
      const generation = get().generation + 1
      set({ generation, connection: "connecting", connectionMessage: null, deltaBuffer: [] })
      return generation
    },
    replace(generation, snapshot) {
      if (generation !== get().generation) return
      set({ snapshot: validateThreadSnapshot(threadId, snapshot), connection: "live", connectionMessage: null, deltaBuffer: [] })
    },
    apply(generation, notification) {
      if (generation !== get().generation) return
      const snapshot = get().snapshot
      if (!snapshot) throw new Error("Thread notification arrived before authoritative snapshot")
      set({ snapshot: applyThreadNotification(snapshot, notification) })
    },
    bufferDelta(generation, notification) {
      if (generation !== get().generation) return
      const payload = notification.notification
      if (payload.type !== "itemDelta") throw new Error("Only Item delta notifications may be buffered")
      set((state) => ({ deltaBuffer: [...state.deltaBuffer, notification] }))
    },
    flushDeltas(generation) {
      if (generation !== get().generation) return
      const buffered = get().deltaBuffer
      if (buffered.length === 0) return
      let snapshot = get().snapshot
      if (!snapshot) throw new Error("Thread delta arrived before authoritative snapshot")
      for (const notification of buffered) snapshot = applyThreadNotification(snapshot, notification)
      set({ snapshot, deltaBuffer: [] })
    },
    setConnection(generation, connection, message) {
      if (generation !== get().generation) return
      set({ connection, connectionMessage: message ?? null })
    },
    invalidate(generation, message) {
      if (generation !== get().generation) return get().generation
      const next = generation + 1
      set({ generation: next, connection: "resyncing", connectionMessage: message, snapshot: null, deltaBuffer: [] })
      return next
    },
  }))
}

export function isItemDelta(notification: ThreadNotificationEnvelope): notification is ThreadNotificationEnvelope & { notification: { type: "itemDelta"; delta: ThreadItemDelta } } {
  return notification.notification.type === "itemDelta"
}
