import { useEffect, useRef } from "react"

import { SessionEventController } from "@/events/session-event-controller"

export function useSessionEvents(sessionId?: string | null) {
  const controller = useRef<SessionEventController | null>(null)
  if (!controller.current) controller.current = new SessionEventController()

  useEffect(() => {
    const current = controller.current
    if (!current) return
    if (sessionId) current.connect(sessionId)
    else current.dispose()
    return () => current.disconnect()
  }, [sessionId])
}
