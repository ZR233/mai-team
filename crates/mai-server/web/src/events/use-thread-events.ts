import { useEffect, useMemo } from "react"
import { useStore } from "zustand"

import { ThreadEventController } from "@/events/thread-event-controller"
import { threadStores } from "@/events/thread-store"

export function useThreadEvents(threadId: string) {
  const store = useMemo(() => threadStores.get(threadId), [threadId])
  useEffect(() => {
    const controller = new ThreadEventController(store)
    controller.connect()
    return () => controller.dispose()
  }, [store])
  return useStore(store)
}
