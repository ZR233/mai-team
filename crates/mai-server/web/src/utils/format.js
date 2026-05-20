export function formatStatus(status) {
  return String(status || 'unknown')
    .replace(/_/g, ' ')
    .replace(/\b\w/g, (letter) => letter.toUpperCase())
}

export function projectReviewRunOutcomeLabel(run = {}) {
  const event = String(run.review_event || '').toLowerCase()
  if (event === 'approve') return 'Approved'
  if (event === 'request_changes') return 'Changes Requested'
  if (event === 'comment') return 'Commented'
  return formatStatus(run.outcome || 'pending')
}

export function projectReviewRunOutcomeTone(run = {}) {
  const event = String(run.review_event || '').toLowerCase()
  if (event === 'approve') return 'approve'
  if (event === 'request_changes') return 'request-changes'
  if (event === 'comment') return 'comment'
  return ''
}

export function formatDate(value) {
  if (!value) return '-'
  return new Date(value).toLocaleString()
}

export function formatTime(value) {
  if (!value) return ''
  return new Date(value).toLocaleTimeString()
}

export function formatEventType(type) {
  return String(type || 'event')
    .replace(/_/g, ' ')
    .replace(/\b\w/g, (l) => l.toUpperCase())
}

export function formatTokenCount(value) {
  return Number(value || 0).toLocaleString()
}

export function numericTokenUsage(value = {}) {
  return {
    input_tokens: Number(value.input_tokens || 0),
    cached_input_tokens: Number(value.cached_input_tokens || 0),
    output_tokens: Number(value.output_tokens || 0),
    reasoning_output_tokens: Number(value.reasoning_output_tokens || 0),
    total_tokens: Number(value.total_tokens || 0)
  }
}

export function sessionTokenUsage(agent) {
  const selectedSessionId = agent?.selected_session_id
  const session = (agent?.sessions || []).find((item) => item.id === selectedSessionId)
  return numericTokenUsage(session?.token_usage || agent?.token_usage || {})
}

export function tokenUsage(agent) {
  const usage = sessionTokenUsage(agent)
  return {
    input: formatTokenCount(usage.input_tokens),
    cachedInput: formatTokenCount(usage.cached_input_tokens),
    output: formatTokenCount(usage.output_tokens),
    reasoningOutput: formatTokenCount(usage.reasoning_output_tokens),
    total: formatTokenCount(usage.total_tokens)
  }
}

export function totalTokens(agent) {
  return tokenUsage(agent).total
}

export function tokenCacheTooltipRows(agent) {
  const usage = sessionTokenUsage(agent)
  const cacheMiss = Math.max(usage.input_tokens - usage.cached_input_tokens, 0)
  const hitRate = usage.input_tokens > 0
    ? Math.round((usage.cached_input_tokens / usage.input_tokens) * 100)
    : 0
  return [
    { label: 'Input', value: formatTokenCount(usage.input_tokens) },
    { label: 'Cache hit', value: formatTokenCount(usage.cached_input_tokens) },
    { label: 'Cache miss', value: formatTokenCount(cacheMiss) },
    { label: 'Hit rate', value: `${hitRate}%` },
    { label: 'Output', value: formatTokenCount(usage.output_tokens) },
    { label: 'Reasoning', value: formatTokenCount(usage.reasoning_output_tokens) },
    { label: 'Total', value: formatTokenCount(usage.total_tokens) }
  ]
}

export function tokenCacheTooltipLabel(agent) {
  const rows = tokenCacheTooltipRows(agent)
  return rows
    .map((row) => `${row.label} ${row.value}`)
    .join(', ')
}

export function tokenBreakdown(agent) {
  const usage = tokenUsage(agent)
  return `Input ${usage.input} · Cached ${usage.cachedInput} · Output ${usage.output} · Reasoning ${usage.reasoningOutput}`
}

export function shortContainer(value) {
  return value ? value.slice(0, 12) : 'none'
}

export function initial(value) {
  return String(value || 'A').charAt(0).toUpperCase()
}

export function roleInitial(role) {
  const value = String(role || '').toLowerCase()
  if (value === 'assistant') return 'A'
  if (value === 'system') return 'S'
  if (value === 'tool') return 'T'
  return 'U'
}

export function roleLabel(role) {
  const value = String(role || '').toLowerCase()
  if (value === 'assistant') return 'Assistant'
  if (value === 'system') return 'System'
  if (value === 'tool') return 'Tool'
  return 'You'
}

export function statusTone(status) {
  const value = String(status || '').toLowerCase()
  if (
    value.includes('run') ||
    value.includes('wait') ||
    value.includes('start') ||
    value.includes('select')
  ) return 'green'
  if (value.includes('fail') || value.includes('error') || value.includes('cancel')) return 'red'
  if (value.includes('creat') || value.includes('delet')) return 'amber'
  if (value.includes('complet')) return 'blue'
  return 'slate'
}

export function messageClass(role) {
  return `message-${String(role || 'user').toLowerCase()}`
}

export function eventSummary(event, formatStatusFn, roleLabelFn) {
  if (event.message) return event.message
  if (event.type === 'agent_created') return `Agent created: ${event.agent?.name || event.agent?.id || ''}`
  if (event.type === 'agent_status_changed') return `${event.agent_id} is ${formatStatusFn(event.status)}`
  if (event.type === 'agent_deleted') return `Agent deleted: ${event.agent_id}`
  if (event.type === 'task_created') return `Task created: ${event.task?.title || event.task?.id || ''}`
  if (event.type === 'task_updated') return `Task updated: ${event.task?.title || event.task?.id || ''}`
  if (event.type === 'task_deleted') return `Task deleted: ${event.task_id}`
  if (event.type === 'turn_started') return `Turn started on ${event.agent_id}`
  if (event.type === 'turn_completed') return `Turn completed with ${formatStatusFn(event.status)}`
  if (event.type === 'tool_started') return `Tool started: ${event.tool_name}`
  if (event.type === 'tool_completed') return `Tool completed: ${event.tool_name}`
  if (event.type === 'context_compacted') return `Context compacted after ${event.tokens_before || 0} tokens`
  if (event.type === 'agent_message') {
    const content = event.content || ''
    return `${roleLabelFn(event.role)}: ${content.length > 120 ? content.slice(0, 120) + '...' : content}`
  }
  if (event.type === 'agent_message_delta') return `${roleLabelFn(event.role)} is responding...`
  if (event.type === 'agent_message_completed') return `${roleLabelFn(event.role)} response completed`
  if (event.type === 'reasoning_delta') return 'Reasoning...'
  if (event.type === 'reasoning_completed') return 'Reasoning completed'
  if (event.type === 'tool_call_delta') return `Tool call prepared: ${event.tool_name || 'tool'}`
  return JSON.stringify(event)
}
