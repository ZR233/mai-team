import { create } from "zustand"

import type { SessionEventEnvelope, SessionViewSnapshot } from "@/events/session-events.generated"
import { applySessionEvent, emptySession, normalizeSnapshot, type NormalizedSession } from "@/events/session-reducer"

export type SessionConnectionState = "idle" | "connecting" | "live" | "resyncing" | "error"

interface SessionStore {
  generation: number
  connection: SessionConnectionState
  connectionMessage: string | null
  view: NormalizedSession
  begin(sessionId: string): number
  replace(generation: number, snapshot: SessionViewSnapshot): void
  apply(generation: number, event: SessionEventEnvelope): void
  setConnection(generation: number, state: SessionConnectionState, message?: string): void
  clear(): void
}

export const useSessionStore = create<SessionStore>((set, get) => ({
  generation: 0,
  connection: "idle",
  connectionMessage: null,
  view: emptySession(),
  begin(sessionId) {
    const current = get()
    const generation = current.generation + 1
    set({
      generation,
      connection: "connecting",
      connectionMessage: null,
      view: current.view.sessionId === sessionId
        ? current.view
        : { ...emptySession(), sessionId },
    })
    return generation
  },
  replace(generation, snapshot) {
    if (generation !== get().generation) return
    set({ view: normalizeSnapshot(snapshot), connection: "live", connectionMessage: null })
  },
  apply(generation, event) {
    if (generation !== get().generation) return
    set((state) => ({ view: applySessionEvent(state.view, event) }))
  },
  setConnection(generation, connection, message) {
    if (generation !== get().generation) return
    set({ connection, connectionMessage: message ?? null })
  },
  clear() {
    set((state) => ({
      generation: state.generation + 1,
      connection: "idle",
      connectionMessage: null,
      view: emptySession(),
    }))
  },
}))
