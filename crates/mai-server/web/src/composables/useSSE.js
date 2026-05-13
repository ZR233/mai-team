import { ref } from 'vue'

const eventFeed = ref([])
const streamingEvents = ref([])
const connectionState = ref('offline')
let eventSource = null
let sseRetryCount = 0
let sseRetryTimer = null
let lastEventId = null

export function useSSE() {
  function connectEvents(onEvent, onReconnect) {
    disconnect()
    connectionState.value = 'connecting'
    const query = lastEventId ? `?last_event_id=${encodeURIComponent(lastEventId)}` : ''
    eventSource = new EventSource(`/events${query}`)
    eventSource.onopen = () => {
      const wasRetrying = sseRetryCount > 0
      connectionState.value = 'online'
      sseRetryCount = 0
      if (wasRetrying && onReconnect) onReconnect()
    }
    eventSource.onerror = () => {
      connectionState.value = 'offline'
      eventSource?.close()
      eventSource = null
      sseRetryCount++
      const delay = Math.min(1000 * Math.pow(2, sseRetryCount - 1), 30000)
      const jitter = Math.floor(Math.random() * 500)
      sseRetryTimer = setTimeout(() => connectEvents(onEvent, onReconnect), delay + jitter)
    }
    const names = [
      'agent_created',
      'agent_updated',
      'agent_status_changed',
      'agent_deleted',
      'task_created',
      'task_updated',
      'task_deleted',
      'project_created',
      'project_updated',
      'project_deleted',
      'turn_started',
      'turn_completed',
      'tool_started',
      'tool_completed',
      'context_compacted',
      'agent_message',
      'agent_message_delta',
      'agent_message_completed',
      'reasoning_delta',
      'reasoning_completed',
      'tool_call_delta',
      'skills_activated',
      'plan_updated',
      'error',
      'todo_list_updated',
      'user_input_requested',
      'artifact_created',
      'mcp_server_status_changed'
    ]
    for (const name of names) {
      eventSource.addEventListener(name, (event) => {
        try {
          const parsed = JSON.parse(event.data)
          if (parsed.sequence) lastEventId = parsed.sequence
          updateStreamingEvents(parsed)
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
    streamingEvents,
    connectionState,
    connectEvents,
    disconnect
  }
}

function updateStreamingEvents(event) {
  if (!event || !isStreamingEvent(event)) return
  const key = streamingKey(event)
  const events = streamingEvents.value.filter((item) => streamingKey(item) !== key)
  const existing = streamingEvents.value.find((item) => streamingKey(item) === key)
  const merged = mergeStreamingEvent(existing, event)
  streamingEvents.value = [merged, ...events].slice(0, 100)
}

function isStreamingEvent(event) {
  return [
    'agent_message_delta',
    'agent_message_completed',
    'reasoning_delta',
    'reasoning_completed',
    'tool_call_delta'
  ].includes(event.type)
}

function streamingKey(event) {
  if (event.type === 'tool_call_delta') {
    return `tool:${event.agent_id || ''}:${event.session_id || ''}:${event.turn_id || ''}:${event.call_id || ''}`
  }
  const channel = event.type.startsWith('reasoning') ? 'reasoning' : (event.channel || 'final')
  return `message:${event.agent_id || ''}:${event.session_id || ''}:${event.turn_id || ''}:${event.message_id || channel}:${channel}`
}

function mergeStreamingEvent(existing, event) {
  if (!existing) return { ...event }
  if (event.type === 'tool_call_delta') {
    return {
      ...existing,
      ...event,
      arguments_delta: `${existing.arguments_delta || ''}${event.arguments_delta || ''}`,
      timestamp: event.timestamp || existing.timestamp,
      sequence: event.sequence || existing.sequence
    }
  }
  if (event.type.endsWith('_delta')) {
    return {
      ...existing,
      ...event,
      delta: `${existing.delta || existing.content || ''}${event.delta || ''}`,
      timestamp: event.timestamp || existing.timestamp,
      sequence: event.sequence || existing.sequence
    }
  }
  return {
    ...existing,
    ...event,
    delta: event.content || existing.delta || '',
    timestamp: event.timestamp || existing.timestamp,
    sequence: event.sequence || existing.sequence
  }
}
