const MESSAGE_MATCH_LIMIT_MS = 120_000

export function buildAgentTimeline(detail, liveEvents = []) {
  if (!detail) return []
  const events = mergeEvents([...(detail.recent_events || []), ...(liveEvents || [])])
    .filter((event) => eventAgentId(event) === detail.id)
    .filter((event) => eventSessionId(event) === detail.selected_session_id || !eventSessionId(event))
    .sort(compareTimelineItems)

  const agentMessageEvents = events.filter((event) => event.type === 'agent_message')
  const matchedMessageCounts = new Map()
  const items = []

  for (const event of agentMessageEvents) {
    const key = messageMatchKey(event.role, event.content)
    matchedMessageCounts.set(key, (matchedMessageCounts.get(key) || 0) + 1)
    items.push({
      type: 'message',
      key: `event-message-${event.sequence || event.timestamp}-${event.role}-${hashText(event.content)}`,
      role: event.role || 'assistant',
      content: event.content || '',
      timestamp: event.timestamp,
      sequence: event.sequence || 0,
      turnId: event.turn_id || null,
      fromEvent: true
    })
  }

  for (const [index, message] of (detail.messages || []).entries()) {
    const key = messageMatchKey(message.role, message.content)
    const matchedCount = matchedMessageCounts.get(key) || 0
    if (matchedCount > 0 && hasNearbyEventMessage(message, agentMessageEvents)) {
      matchedMessageCounts.set(key, matchedCount - 1)
      continue
    }
    items.push({
      type: 'message',
      key: `persisted-message-${message.created_at}-${message.role}-${index}`,
      role: message.role || 'assistant',
      content: message.content || '',
      timestamp: message.created_at,
      sequence: index,
      turnId: null,
      fromEvent: false
    })
  }

  const firstMessageByTurn = new Map()
  for (const item of items) {
    if (item.turnId && !firstMessageByTurn.has(item.turnId)) {
      firstMessageByTurn.set(item.turnId, item)
    }
  }

  const tools = new Map()
  for (const event of events) {
    if (event.type === 'turn_started') {
      const anchor = firstMessageByTurn.get(event.turn_id)
      items.push({
        type: 'process',
        key: `thinking-${event.turn_id}-${event.sequence || event.timestamp}`,
        tone: 'active',
        label: 'Thinking',
        detail: 'Preparing the next step',
        timestamp: anchor ? offsetTimestamp(anchor.timestamp, 1) : event.timestamp,
        sequence: (anchor?.sequence || event.sequence || 0) + 0.1
      })
    } else if (event.type === 'turn_completed') {
      items.push({
        type: 'process',
        key: `turn-completed-${event.turn_id}-${event.sequence || event.timestamp}`,
        tone: event.status === 'failed' ? 'error' : event.status === 'cancelled' ? 'warning' : 'done',
        label: formatTurnStatus(event.status),
        detail: 'Turn completed',
        timestamp: event.timestamp,
        sequence: event.sequence || 0
      })
    } else if (event.type === 'error') {
      items.push({
        type: 'error',
        key: `error-${event.sequence || event.timestamp}`,
        message: event.message || 'Unknown error',
        timestamp: event.timestamp,
        sequence: event.sequence || 0
      })
    } else if (event.type === 'agent_status_changed') {
      const row = statusProcessRow(event)
      if (row) items.push(row)
    } else if (event.type === 'tool_started') {
      const tool = tools.get(event.call_id) || baseTool(event)
      Object.assign(tool, {
        started: event,
        timestamp: event.timestamp,
        sequence: event.sequence || 0,
        argumentsPreview: event.arguments_preview || previewValue(event.arguments),
        arguments: event.arguments || null,
        status: 'running'
      })
      tools.set(event.call_id, tool)
    } else if (event.type === 'tool_completed') {
      const tool = tools.get(event.call_id) || baseTool(event)
      Object.assign(tool, {
        completed: event,
        outputPreview: event.output_preview || '',
        durationMs: event.duration_ms ?? null,
        success: Boolean(event.success),
        status: event.success ? 'completed' : 'failed'
      })
      tools.set(event.call_id, tool)
      items.push({
        type: 'process',
        key: `reading-${event.call_id}-${event.sequence || event.timestamp}`,
        tone: event.success ? 'active' : 'error',
        label: event.success ? 'Reading output' : 'Tool returned an error',
        detail: event.tool_name || 'tool',
        timestamp: offsetTimestamp(event.timestamp, 1),
        sequence: (event.sequence || 0) + 0.1
      })
    }
  }

  for (const tool of tools.values()) {
    items.push({
      type: 'tool',
      key: `tool-${tool.callId}`,
      callId: tool.callId,
      turnId: tool.turnId,
      toolName: tool.toolName,
      status: tool.status,
      success: tool.success,
      argumentsPreview: tool.argumentsPreview,
      outputPreview: tool.outputPreview,
      durationMs: tool.durationMs,
      timestamp: tool.timestamp,
      sequence: tool.sequence
    })
  }

  return items.sort(compareTimelineItems)
}

export function formatDuration(ms) {
  if (ms === null || ms === undefined) return ''
  if (ms < 1000) return `${ms} ms`
  return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} s`
}

export function formatTraceValue(value) {
  if (value === null || value === undefined || value === '') return ''
  if (typeof value === 'string') {
    try {
      return JSON.stringify(JSON.parse(value), null, 2)
    } catch {
      return value
    }
  }
  return JSON.stringify(value, null, 2)
}

export function toolStatusLabel(status) {
  if (status === 'running') return 'Running'
  if (status === 'failed') return 'Failed'
  return 'Done'
}

function mergeEvents(events) {
  const seen = new Set()
  const merged = []
  for (const event of events) {
    if (!event) continue
    const key = event.sequence
      ? `seq:${event.sequence}`
      : `${event.type}:${event.call_id || event.turn_id || event.agent_id || ''}:${event.timestamp || ''}`
    if (seen.has(key)) continue
    seen.add(key)
    merged.push(event)
  }
  return merged
}

function eventAgentId(event) {
  if (event.type === 'agent_created') return event.agent?.id
  return event.agent_id || null
}

function eventSessionId(event) {
  return event.session_id || null
}

function baseTool(event) {
  return {
    callId: event.call_id,
    turnId: event.turn_id || null,
    toolName: event.tool_name || 'tool',
    status: 'running',
    success: null,
    argumentsPreview: '',
    outputPreview: '',
    durationMs: null,
    timestamp: event.timestamp,
    sequence: event.sequence || 0
  }
}

function statusProcessRow(event) {
  const status = String(event.status || '')
  if (!['starting_container', 'failed', 'cancelled', 'deleting_container', 'deleted'].includes(status)) {
    return null
  }
  return {
    type: 'process',
    key: `status-${event.sequence || event.timestamp}`,
    tone: status.includes('fail') ? 'error' : status.includes('cancel') ? 'warning' : 'muted',
    label: status.replace(/_/g, ' ').replace(/\b\w/g, (letter) => letter.toUpperCase()),
    detail: 'Status changed',
    timestamp: event.timestamp,
    sequence: event.sequence || 0
  }
}

function hasNearbyEventMessage(message, events) {
  const messageTime = toTime(message.created_at)
  return events.some((event) => {
    if (messageMatchKey(event.role, event.content) !== messageMatchKey(message.role, message.content)) return false
    return Math.abs(toTime(event.timestamp) - messageTime) <= MESSAGE_MATCH_LIMIT_MS
  })
}

function messageMatchKey(role, content) {
  return `${String(role || '').toLowerCase()}:${String(content || '')}`
}

function previewValue(value) {
  if (value === null || value === undefined) return ''
  return formatTraceValue(value).replace(/\n/g, '\\n').slice(0, 500)
}

function compareTimelineItems(left, right) {
  const timeDelta = toTime(left.timestamp) - toTime(right.timestamp)
  if (timeDelta !== 0) return timeDelta
  return Number(left.sequence || 0) - Number(right.sequence || 0)
}

function toTime(timestamp) {
  const time = new Date(timestamp || 0).getTime()
  return Number.isFinite(time) ? time : 0
}

function offsetTimestamp(timestamp, ms) {
  return new Date(toTime(timestamp) + ms).toISOString()
}

function formatTurnStatus(status) {
  if (status === 'completed') return 'Completed'
  if (status === 'cancelled') return 'Cancelled'
  if (status === 'failed') return 'Failed'
  return 'Turn completed'
}

function hashText(value) {
  let hash = 0
  for (let index = 0; index < String(value || '').length; index += 1) {
    hash = ((hash << 5) - hash + String(value).charCodeAt(index)) | 0
  }
  return Math.abs(hash).toString(36)
}
