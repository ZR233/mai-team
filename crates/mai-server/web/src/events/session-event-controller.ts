import type { SessionEventEnvelope, SessionStreamFrame } from "@/events/session-events.generated"
import { useSessionStore } from "@/events/session-store"

export class SessionEventController {
  private source: EventSource | null = null
  private animationFrame: number | null = null
  private deltas: SessionEventEnvelope[] = []

  connect(sessionId: string, bootstrap: "resume" | "snapshot" = "resume") {
    this.disconnect()
    const store = useSessionStore.getState()
    const cursor = bootstrap === "resume" && store.view.sessionId === sessionId
      ? store.view.throughSequence
      : 0
    const generation = store.begin(sessionId)
    const query = cursor > 0 ? `?after_sequence=${cursor}` : ""
    const source = new EventSource(`/sessions/${encodeURIComponent(sessionId)}/events${query}`)
    this.source = source

    const consume = (message: MessageEvent<string>) => {
      if (generation !== useSessionStore.getState().generation) return
      try {
        this.consumeFrame(generation, JSON.parse(message.data) as SessionStreamFrame)
      } catch (error) {
        this.resync(generation, error instanceof Error ? error.message : "Invalid session event")
      }
    }
    source.addEventListener("snapshot", consume as EventListener)
    source.addEventListener("event", consume as EventListener)
    source.addEventListener("resyncRequired", consume as EventListener)
    source.onopen = () => useSessionStore.getState().setConnection(generation, "live")
    source.onerror = () => {
      if (source.readyState === EventSource.CLOSED) {
        useSessionStore.getState().setConnection(generation, "error", "Session stream disconnected")
      } else {
        useSessionStore.getState().setConnection(generation, "connecting", "Reconnecting…")
      }
    }
  }

  disconnect() {
    this.source?.close()
    this.source = null
    this.deltas = []
    if (this.animationFrame !== null) cancelAnimationFrame(this.animationFrame)
    this.animationFrame = null
  }

  dispose() {
    this.disconnect()
    useSessionStore.getState().clear()
  }

  private consumeFrame(generation: number, frame: SessionStreamFrame) {
    switch (frame.type) {
      case "snapshot":
        this.flush(generation)
        useSessionStore.getState().replace(generation, frame.snapshot)
        return
      case "event":
        if (frame.event.kind.type === "partDelta") {
          this.deltas.push(frame.event)
          this.scheduleFlush(generation)
        } else {
          this.flush(generation)
          useSessionStore.getState().apply(generation, frame.event)
        }
        return
      case "resyncRequired":
        this.resync(generation, `Resync required: ${frame.reason.type}`)
        return
    }
  }

  private scheduleFlush(generation: number) {
    if (this.animationFrame !== null) return
    this.animationFrame = requestAnimationFrame(() => {
      this.animationFrame = null
      try {
        this.flush(generation)
      } catch (error) {
        this.resync(generation, error instanceof Error ? error.message : "Invalid session delta")
      }
    })
  }

  private flush(generation: number) {
    const events = this.deltas
    this.deltas = []
    for (const event of events) useSessionStore.getState().apply(generation, event)
  }

  private resync(generation: number, message: string) {
    if (generation !== useSessionStore.getState().generation) return
    useSessionStore.getState().setConnection(generation, "resyncing", message)
    const sessionId = useSessionStore.getState().view.sessionId
    this.disconnect()
    if (sessionId) window.setTimeout(() => this.connect(sessionId, "snapshot"), 0)
  }
}
