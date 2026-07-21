import { beforeEach, describe, expect, it } from "vitest"

import { emptySession } from "@/events/session-reducer"
import { durableEvent, sessionSnapshot } from "@/events/session-test-fixtures"
import { useSessionStore } from "@/events/session-store"

describe("session store generations", () => {
  beforeEach(() => {
    useSessionStore.setState({
      generation: 0,
      connection: "idle",
      connectionMessage: null,
      view: emptySession(),
    })
  })

  it("does not let an old session generation update the selected session", () => {
    const store = useSessionStore.getState()
    const generationA = store.begin("session-a")
    useSessionStore.getState().replace(generationA, sessionSnapshot("session-a"))
    const generationB = useSessionStore.getState().begin("session-b")
    useSessionStore.getState().replace(generationB, sessionSnapshot("session-b"))

    useSessionStore.getState().apply(generationA, durableEvent({
      type: "errorOccurred",
      message: "stale",
      severity: "recoverable",
    }, 1, "session-a"))

    expect(useSessionStore.getState().view.sessionId).toBe("session-b")
    expect(useSessionStore.getState().view.lastError).toBeNull()
  })
})
