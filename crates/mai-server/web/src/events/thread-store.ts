import { createStore, type StoreApi } from "zustand/vanilla"

import type { ThreadNotificationEnvelope, ThreadSnapshot } from "@/events/thread-events.generated"
import { applyThreadNotification, ThreadProjectionError, validateThreadSnapshot } from "@/events/thread-reducer"

export type ThreadConnectionState = "idle" | "connecting" | "live" | "resyncing" | "error" | "closed"

export interface ThreadStoreState {
  threadId: string
  generation: number
  connection: ThreadConnectionState
  connectionMessage: string | null
  /** 权威首帧；历史 Turn/items 不在这里，由 `/threads/{id}/turns` 单独读取。 */
  snapshot: ThreadSnapshot | null
  /** 当前广播生命周期水位；首帧后、收到第一条通知前为 null（未知）。 */
  epoch: number | null
  begin(): number
  replace(generation: number, snapshot: ThreadSnapshot): void
  apply(generation: number, notification: ThreadNotificationEnvelope): void
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
    epoch: null,
    begin() {
      const generation = get().generation + 1
      set({ generation, connection: "connecting", connectionMessage: null, epoch: null })
      return generation
    },
    replace(generation, snapshot) {
      if (generation !== get().generation) return
      set({ snapshot: validateThreadSnapshot(threadId, snapshot), epoch: null, connection: "live", connectionMessage: null })
    },
    apply(generation, notification) {
      if (generation !== get().generation) return
      const snapshot = get().snapshot
      if (!snapshot) throw new ThreadProjectionError("Thread notification arrived before authoritative snapshot")
      const epoch = get().epoch
      if (epoch !== null && epoch !== notification.epoch) {
        throw new ThreadProjectionError(`Thread broadcast epoch changed from ${epoch} to ${notification.epoch}`)
      }
      set({ snapshot: applyThreadNotification(snapshot, notification), epoch: notification.epoch })
    },
    setConnection(generation, connection, message) {
      if (generation !== get().generation) return
      set({ connection, connectionMessage: message ?? null })
    },
    invalidate(generation, message) {
      if (generation !== get().generation) return get().generation
      const next = generation + 1
      set({ generation: next, connection: "resyncing", connectionMessage: message, snapshot: null, epoch: null })
      return next
    },
  }))
}
