import { ref } from 'vue'

const eventFeed = ref([])
const connectionState = ref('offline')
let eventSource = null
let sseRetryCount = 0
let sseRetryTimer = null
const SSE_MAX_RETRIES = 5

export function useSSE() {
  function connectEvents(token, onEvent) {
    disconnect()
    if (!token) {
      connectionState.value = 'offline'
      return
    }
    connectionState.value = 'connecting'
    eventSource = new EventSource(`/events?token=${encodeURIComponent(token)}`)
    eventSource.onopen = () => {
      connectionState.value = 'online'
      sseRetryCount = 0
    }
    eventSource.onerror = () => {
      connectionState.value = 'offline'
      eventSource?.close()
      eventSource = null
      if (sseRetryCount < SSE_MAX_RETRIES) {
        sseRetryCount++
        const delay = Math.min(1000 * Math.pow(2, sseRetryCount - 1), 30000)
        sseRetryTimer = setTimeout(() => connectEvents(token, onEvent), delay)
      } else if (onEvent) {
        onEvent({ type: 'sse_auth_required' })
      }
    }
    const names = [
      'agent_created',
      'agent_status_changed',
      'agent_deleted',
      'turn_started',
      'turn_completed',
      'tool_started',
      'tool_completed',
      'agent_message',
      'error'
    ]
    for (const name of names) {
      eventSource.addEventListener(name, (event) => {
        try {
          const parsed = JSON.parse(event.data)
          eventFeed.value = [parsed, ...eventFeed.value].slice(0, 150)
          if (onEvent) onEvent(parsed)
        } catch {
          const fallback = { sequence: Date.now(), type: 'event', timestamp: new Date().toISOString(), message: event.data }
          eventFeed.value = [fallback, ...eventFeed.value].slice(0, 150)
        }
      })
    }
  }

  function disconnect() {
    if (eventSource) {
      eventSource.close()
      eventSource = null
    }
    if (sseRetryTimer) {
      clearTimeout(sseRetryTimer)
      sseRetryTimer = null
    }
  }

  function resetRetryCount() {
    sseRetryCount = 0
  }

  return {
    eventFeed,
    connectionState,
    connectEvents,
    disconnect,
    resetRetryCount
  }
}
