import { ref } from 'vue'

const eventFeed = ref([])
const connectionState = ref('offline')
let eventSource = null
let sseRetryCount = 0
let sseRetryTimer = null
const SSE_MAX_RETRIES = 5

export function useSSE() {
  function connectEvents(onEvent) {
    disconnect()
    connectionState.value = 'connecting'
    eventSource = new EventSource('/events')
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
        sseRetryTimer = setTimeout(() => connectEvents(onEvent), delay)
      }
    }
    const names = [
      'agent_created',
      'agent_updated',
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

  return {
    eventFeed,
    connectionState,
    connectEvents,
    disconnect
  }
}
