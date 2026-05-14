import hljs from 'highlight.js/lib/common'

const MESSAGE_MATCH_LIMIT_MS = 120_000

export function buildAgentTimeline(detail, liveEvents = []) {
  if (!detail) return []
  const events = mergeEvents([...(detail.recent_events || []), ...(liveEvents || [])])
    .filter((event) => eventAgentId(event) === detail.id)
    .filter((event) => eventSessionId(event) === detail.selected_session_id || !eventSessionId(event))
    .sort(compareTimelineItems)

  const agentMessageEvents = events.filter((event) => event.type === 'agent_message')
  const streamMessages = buildStreamingMessages(events)
  const matchedMessageCounts = new Map()
  const items = []

  for (const event of agentMessageEvents) {
    if (streamMessages.finalCompletedKeys.has(streamMessageKey(event))) {
      continue
    }
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

  for (const stream of streamMessages.items) {
    items.push(stream)
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
        title: errorTitle(event.message),
        message: event.message || 'Unknown error',
        timestamp: event.timestamp,
        sequence: event.sequence || 0
      })
    } else if (event.type === 'context_compacted') {
      items.push({
        type: 'context_event',
        key: `context-compacted-${event.turn_id}-${event.sequence || event.timestamp}`,
        tone: 'done',
        title: 'Context compacted',
        summary: contextCompactionSummary(event),
        tokensBefore: event.tokens_before ?? null,
        tokensAfter: event.tokens_after ?? null,
        turnId: event.turn_id || null,
        timestamp: event.timestamp,
        sequence: event.sequence || 0
      })
    } else if (event.type === 'skills_activated') {
      const skills = (event.skills || []).map(skillInfo).filter((skill) => skill.name)
      if (skills.length) {
        const anchor = firstMessageByTurn.get(event.turn_id)
        items.push({
          type: 'skill_call',
          key: `skills-activated-${event.turn_id}-${event.sequence || event.timestamp}`,
          tone: 'skill',
          title: 'Skills activated',
          summary: `${skills.length} ${skills.length === 1 ? 'skill' : 'skills'} loaded`,
          skills,
          turnId: event.turn_id || null,
          timestamp: anchor ? offsetTimestamp(anchor.timestamp, 0.5) : event.timestamp,
          sequence: (anchor?.sequence || event.sequence || 0) + 0.05
        })
      }
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
    } else if (event.type === 'tool_call_delta') {
      const tool = tools.get(event.call_id) || baseTool({
        ...event,
        tool_name: event.tool_name || 'tool'
      })
      Object.assign(tool, {
        timestamp: event.timestamp,
        sequence: event.sequence || 0,
        argumentsPreview: `${tool.argumentsPreview || ''}${event.arguments_delta || ''}`,
        status: 'preparing'
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
        tone: event.success ? 'active' : 'warning',
        label: event.success ? 'Reading output' : 'Command exited with an error',
        detail: event.tool_name || 'tool',
        timestamp: offsetTimestamp(event.timestamp, 1),
        sequence: (event.sequence || 0) + 0.1
      })
    } else if (event.type === 'todo_list_updated') {
      items.push({
        type: 'todo_list',
        key: `todo-${event.turn_id}-${event.sequence || event.timestamp}`,
        items: (event.items || []).map((item) => ({
          step: item.step || '',
          status: item.status || 'pending'
        })),
        timestamp: event.timestamp,
        sequence: event.sequence || 0
      })
    } else if (event.type === 'user_input_requested') {
      items.push({
        type: 'user_input',
        key: `user-input-${event.turn_id}-${event.sequence || event.timestamp}`,
        header: event.header || '',
        questions: (event.questions || []).map((q) => ({
          id: q.id || '',
          question: q.question || '',
          options: (q.options || []).map((o) => ({
            label: o.label || '',
            description: o.description || ''
          }))
        })),
        timestamp: event.timestamp,
        sequence: event.sequence || 0
      })
    } else if (event.type === 'artifact_created') {
      items.push({
        type: 'artifact',
        key: `artifact-${event.artifact.id}`,
        artifact: event.artifact,
        timestamp: event.timestamp,
        sequence: event.sequence || 0
      })
    }
  }

  for (const tool of tools.values()) {
    const summary = summarizeTool(tool)
    items.push({
      type: 'tool_call',
      key: `tool-${tool.callId}`,
      callId: tool.callId,
      turnId: tool.turnId,
      toolName: tool.toolName,
      status: tool.status,
      success: tool.success,
      argumentsPreview: tool.argumentsPreview,
      outputPreview: tool.outputPreview,
      toolActionLabel: summary.actionLabel,
      toolPrimary: summary.primary,
      toolSecondary: summary.secondary,
      toolPreviewLines: summary.previewLines,
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

export function renderTraceValue(value) {
  const formatted = cleanTerminalText(formatTraceValue(value))
  if (!formatted) return ''

  if (isJsonTraceValue(value, formatted)) {
    try {
      return hljs.highlight(formatted, { language: 'json', ignoreIllegals: true }).value
    } catch {
      return escapeHtml(formatted)
    }
  }

  return escapeHtml(formatted)
}

export function renderToolTrace({ toolName, kind, value }) {
  const normalizedToolName = normalizeToolName(toolName)
  const parsed = parseTraceValue(value)

  if (kind === 'arguments') {
    return renderToolArguments(normalizedToolName, parsed.value)
  }

  if (kind === 'output') {
    return renderToolOutput(normalizedToolName, parsed.value)
  }

  return `<pre class="trace-code trace-code-${parsed.type}"><code>${renderTraceValue(value)}</code></pre>`
}

export function toolStatusLabel(status) {
  if (status === 'preparing') return 'Preparing'
  if (status === 'running') return 'Running'
  if (status === 'failed') return 'Failed'
  return 'Done'
}

function renderToolArguments(toolName, value) {
  if (isPlainObject(value)) {
    if (toolName === 'container_exec' && typeof value.command === 'string') {
      const rows = [
        renderCommandBlock(value.command),
        value.cwd ? renderMetaLine('cwd', value.cwd) : '',
        value.timeout_secs ? renderMetaLine('timeout', `${value.timeout_secs}s`) : ''
      ]
      return rows.filter(Boolean).join('')
    }

    if (toolName === 'container_cp_upload') {
      return [
        renderMetaLine('path', value.path),
        renderMetaLine('content', compactBase64(value.content_base64))
      ].filter(Boolean).join('')
    }

    if (toolName === 'container_cp_download') {
      return renderMetaLine('path', value.path)
    }

    if (toolName === 'send_message') {
      return [
        renderMetaLine('agent', value.agent_id),
        value.session_id ? renderMetaLine('session', value.session_id) : '',
        renderTextBlock('message', value.message)
      ].filter(Boolean).join('')
    }

    if (toolName === 'spawn_agent') {
      return [
        renderMetaLine('name', value.name),
        value.provider_id ? renderMetaLine('provider', value.provider_id) : '',
        value.model ? renderMetaLine('model', value.model) : '',
        renderTextBlock('message', value.message)
      ].filter(Boolean).join('')
    }

    if (toolName === 'wait_agent' || toolName === 'close_agent') {
      return [
        renderMetaLine('agent', value.agent_id),
        value.timeout_secs ? renderMetaLine('timeout', `${value.timeout_secs}s`) : ''
      ].filter(Boolean).join('')
    }

    return renderGenericTraceSections(value)
  }

  return renderGenericTraceSections(value)
}

function renderToolOutput(toolName, value) {
  if (toolName === 'container_exec' && isPlainObject(value) && hasExecOutputShape(value)) {
    const rows = [
      renderStatusLine(value.status),
      renderTextBlock('stdout', value.stdout),
      renderTextBlock('stderr', value.stderr, 'error')
    ]
    return rows.filter(Boolean).join('')
  }

  if (isPlainObject(value)) {
    if ('path' in value || 'bytes' in value) {
      return [
        renderMetaLine('path', value.path),
        Number.isFinite(value.bytes) ? renderMetaLine('bytes', formatBytes(value.bytes)) : ''
      ].filter(Boolean).join('')
    }

    if ('error' in value || 'message' in value) {
      return [
        renderTextBlock('error', value.error, 'error'),
        renderTextBlock('message', value.message)
      ].filter(Boolean).join('')
    }
  }

  if (typeof value === 'string') {
    return renderTextBlock('output', decodeEscapedText(value))
  }

  return renderGenericTraceSections(value)
}

function renderCommandBlock(command) {
  return `
    <div class="trace-command">
      <span class="trace-prompt">$</span>
      <code>${escapeHtml(cleanTerminalText(command))}</code>
    </div>
  `
}

function renderStatusLine(status) {
  if (status === null || status === undefined || status === '') return ''
  const numericStatus = Number(status)
  const ok = numericStatus === 0
  const label = cleanTerminalText(String(status))
  return `
    <div class="trace-status-line">
      <span class="trace-status-pill ${ok ? 'trace-status-ok' : 'trace-status-error'}">
        exit ${escapeHtml(label)}
      </span>
    </div>
  `
}

function renderMetaLine(label, value) {
  if (value === null || value === undefined || value === '') return ''
  return `
    <div class="trace-meta-line">
      <span>${escapeHtml(cleanTerminalText(String(label)))}</span>
      <code>${escapeHtml(cleanTerminalText(String(value)))}</code>
    </div>
  `
}

function renderTextBlock(label, value, tone = '') {
  if (value === null || value === undefined || value === '') return ''
  const text = cleanTerminalText(String(value))
  if (!text) return ''
  return `
    <div class="trace-text-block ${tone ? `trace-text-${tone}` : ''}">
      <span>${escapeHtml(cleanTerminalText(String(label)))}</span>
      <pre>${escapeHtml(text)}</pre>
    </div>
  `
}

function renderGenericTraceSections(value) {
  if (value === null || value === undefined || value === '') return ''
  if (isPlainObject(value)) {
    return Object.entries(value)
      .map(([key, item]) => renderGenericTraceEntry(key, item))
      .filter(Boolean)
      .join('')
  }
  if (Array.isArray(value)) return renderMetaLine('items', summarizeTraceCollection(value))
  return renderMetaLine('value', value)
}

function renderGenericTraceEntry(key, value) {
  if (value === null || value === undefined || value === '') return ''
  if (isTraceScalar(value)) return renderMetaLine(key, value)
  if (Array.isArray(value)) return renderMetaLine(key, summarizeTraceCollection(value))
  if (isPlainObject(value)) return renderMetaLine(key, summarizeTraceObject(value))
  return renderMetaLine(key, String(value))
}

function parseTraceValue(value) {
  if (value === null || value === undefined || value === '') return { type: 'text', value: '' }
  if (typeof value !== 'string') return { type: 'json', value }

  const decoded = decodeEscapedText(value)
  for (const candidate of [value, decoded]) {
    try {
      return { type: 'json', value: JSON.parse(candidate) }
    } catch { }
  }
  return { type: 'text', value: decoded }
}

function decodeEscapedText(value) {
  const text = String(value || '')
  if (!/\\[nrt"\\]/.test(text)) return text
  return text
    .replace(/\\r\\n/g, '\n')
    .replace(/\\n/g, '\n')
    .replace(/\\r/g, '\r')
    .replace(/\\t/g, '\t')
    .replace(/\\"/g, '"')
    .replace(/\\\\/g, '\\')
}

function cleanTerminalText(value) {
  return decodeEscapedText(value)
    .replace(/\u001b\[[0-?]*[ -/]*[@-~]/g, '')
    .replace(/\u001b\][^\u0007]*(?:\u0007|\u001b\\)/g, '')
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, '')
}

function normalizeToolName(value) {
  return String(value || '').replace(/\./g, '_')
}

function isPlainObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function isTraceScalar(value) {
  return ['string', 'number', 'boolean', 'bigint'].includes(typeof value)
}

function hasExecOutputShape(value) {
  return 'status' in value && ('stdout' in value || 'stderr' in value)
}

function compactBase64(value) {
  if (!value) return ''
  const text = String(value)
  if (text.length <= 36) return text
  return `${text.slice(0, 18)}...${text.slice(-10)} (${text.length} chars)`
}

function formatBytes(value) {
  if (!Number.isFinite(value)) return ''
  if (value < 1024) return `${value} B`
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`
  return `${(value / 1024 / 1024).toFixed(1)} MB`
}

function isJsonTraceValue(original, formatted) {
  if (original !== null && typeof original === 'object') return true
  if (typeof original !== 'string') return original !== undefined
  try {
    JSON.parse(formatted)
    return true
  } catch {
    return looksLikeJsonText(formatted)
  }
}

function looksLikeJsonText(value) {
  const trimmed = String(value || '').trimStart()
  return trimmed.startsWith('{') || trimmed.startsWith('[')
}

function escapeHtml(value) {
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;')
}

function mergeEvents(events) {
  const seen = new Set()
  const merged = []
  for (const event of events) {
    if (!event) continue
    const key = streamingEventMergeKey(event) || (event.sequence
      ? `seq:${event.sequence}`
      : `${event.type}:${event.call_id || event.turn_id || event.agent_id || ''}:${event.timestamp || ''}`)
    if (seen.has(key)) continue
    seen.add(key)
    merged.push(event)
  }
  return merged
}

function streamingEventMergeKey(event) {
  if (event.type === 'tool_call_delta') {
    return `stream-tool:${event.agent_id || ''}:${event.session_id || ''}:${event.turn_id || ''}:${event.call_id || ''}`
  }
  if (!['agent_message_delta', 'agent_message_completed', 'reasoning_delta', 'reasoning_completed'].includes(event.type)) {
    return null
  }
  const channel = event.type.startsWith('reasoning') ? 'reasoning' : (event.channel || 'final')
  return `stream-message:${event.agent_id || ''}:${event.session_id || ''}:${event.turn_id || ''}:${event.message_id || channel}:${channel}`
}

function buildStreamingMessages(events) {
  const streams = new Map()
  const finalCompletedKeys = new Set()
  for (const event of events) {
    if (!['agent_message_delta', 'agent_message_completed', 'reasoning_delta', 'reasoning_completed'].includes(event.type)) {
      continue
    }
    const channel = event.type.startsWith('reasoning') ? 'reasoning' : (event.channel || 'final')
    const key = `${event.turn_id || ''}:${event.message_id || channel}:${channel}`
    const stream = streams.get(key) || {
      type: channel === 'reasoning' ? 'process' : 'message',
      key: `stream-${key}`,
      role: event.role || 'assistant',
      content: '',
      tone: channel === 'reasoning' ? 'active' : undefined,
      label: channel === 'reasoning' ? 'Reasoning' : undefined,
      detail: '',
      timestamp: event.timestamp,
      sequence: event.sequence || 0,
      turnId: event.turn_id || null,
      fromEvent: true,
      streaming: true,
      channel
    }
    if (event.type.endsWith('_delta')) {
      stream.content = `${stream.content || ''}${event.delta || ''}`
      stream.detail = stream.content
    } else {
      stream.content = event.content || stream.content || ''
      stream.detail = stream.content
      stream.streaming = false
      if (channel === 'final') {
        finalCompletedKeys.add(streamMessageKey(event))
      }
    }
    stream.timestamp = event.timestamp || stream.timestamp
    stream.sequence = event.sequence || stream.sequence
    streams.set(key, stream)
  }
  return {
    items: [...streams.values()].filter((item) => (item.content || item.detail || '').trim()),
    finalCompletedKeys
  }
}

function streamMessageKey(event) {
  return `${event.turn_id || ''}:${event.role || 'assistant'}:${event.content || ''}`
}

function eventAgentId(event) {
  if (event.type === 'agent_created') return event.agent?.id
  if (event.type === 'artifact_created') return event.artifact?.agent_id || null
  return event.agent_id || null
}

function eventSessionId(event) {
  return event.session_id || null
}

function skillInfo(skill) {
  return {
    name: cleanOneLine(skill?.display_name || skill?.name || ''),
    scope: cleanOneLine(skill?.scope || ''),
    path: cleanOneLine(skill?.path || '')
  }
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

function summarizeTool(tool) {
  const toolName = normalizeToolName(tool.toolName)
  const args = parseTraceValue(tool.arguments || tool.argumentsPreview).value
  const output = parseTraceValue(tool.outputPreview).value

  if (toolName === 'container_exec' && isPlainObject(args)) {
    return {
      actionLabel: tool.status === 'preparing' ? 'Preparing' : tool.status === 'running' ? 'Running' : tool.status === 'failed' ? 'Failed' : 'Ran',
      primary: cleanOneLine(args.command || tool.toolName),
      secondary: args.cwd ? `cwd ${cleanOneLine(args.cwd)}` : '',
      previewLines: previewOutputLines(output)
    }
  }

  return {
    actionLabel: tool.status === 'preparing' ? 'Preparing' : tool.status === 'running' ? 'Calling' : tool.status === 'failed' ? 'Failed' : 'Called',
    primary: tool.toolName,
    secondary: summarizeToolArgs(args),
    previewLines: previewOutputLines(output)
  }
}

function summarizeToolArgs(value) {
  if (!isPlainObject(value)) return cleanOneLine(formatTraceValue(value)).slice(0, 120)
  const preferredKeys = ['path', 'agent_id', 'session_id', 'name', 'provider_id', 'model', 'timeout_secs']
  const entries = preferredKeys
    .filter((key) => value[key] !== null && value[key] !== undefined && value[key] !== '')
    .map((key) => `${key.replace(/_id$/, '')} ${cleanOneLine(String(value[key]))}`)
  if (entries.length) return entries.slice(0, 2).join(' · ')
  const keys = Object.keys(value)
  if (!keys.length) return ''
  return keys.slice(0, 3).join(', ')
}

function previewOutputLines(value) {
  const text = outputText(value)
  if (!text) return []
  const lines = text.split('\n').map((line) => line.trimEnd()).filter(Boolean)
  return lines.slice(-4).map((line) => (line.length > 180 ? `${line.slice(0, 177)}...` : line))
}

function outputText(value) {
  if (value === null || value === undefined || value === '') return ''
  if (typeof value === 'string') return cleanTerminalText(value)
  if (isPlainObject(value)) {
    const parts = []
    if (value.stdout) parts.push(String(value.stdout))
    if (value.stderr) parts.push(String(value.stderr))
    if (parts.length) return cleanTerminalText(parts.join('\n'))
    if (value.message) return cleanTerminalText(String(value.message))
    if (value.error) return cleanTerminalText(String(value.error))
  }
  return cleanTerminalText(formatTraceValue(value))
}

function summarizeTraceCollection(value) {
  const items = value.slice(0, 3).map(summarizeTraceValue).filter(Boolean)
  const suffix = value.length > items.length ? `, +${value.length - items.length} more` : ''
  return `[${items.join(', ')}${suffix}]`
}

function summarizeTraceObject(value) {
  const keys = Object.keys(value)
  if (!keys.length) return '{}'
  const preview = keys.slice(0, 4).join(', ')
  const suffix = keys.length > 4 ? `, +${keys.length - 4} more` : ''
  return `{${preview}${suffix}}`
}

function summarizeTraceValue(value) {
  if (value === null) return 'null'
  if (value === undefined) return ''
  if (isTraceScalar(value)) return cleanOneLine(String(value)).slice(0, 80)
  if (Array.isArray(value)) return `${value.length} items`
  if (isPlainObject(value)) return summarizeTraceObject(value)
  return cleanOneLine(String(value)).slice(0, 80)
}

function cleanOneLine(value) {
  return cleanTerminalText(String(value || '')).replace(/\s+/g, ' ').trim()
}

function contextCompactionSummary(event) {
  const before = event.tokens_before
  const after = event.tokens_after
  if (Number.isFinite(before) && Number.isFinite(after)) return `${before} to ${after} tokens`
  if (Number.isFinite(before)) return `${before} tokens before compaction`
  if (Number.isFinite(after)) return `${after} tokens after compaction`
  return 'Conversation context summarized'
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

function errorTitle(message) {
  const text = String(message || '').toLowerCase()
  if (text.includes('model error') || text.includes('/chat/completions') || text.includes('/responses')) {
    return 'Model request failed'
  }
  return 'Error'
}

function hashText(value) {
  let hash = 0
  for (let index = 0; index < String(value || '').length; index += 1) {
    hash = ((hash << 5) - hash + String(value).charCodeAt(index)) | 0
  }
  return Math.abs(hash).toString(36)
}
