export function formatStatus(status) {
  return String(status || 'unknown')
    .replace(/_/g, ' ')
    .replace(/\b\w/g, (letter) => letter.toUpperCase())
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

export function totalTokens(agent) {
  return Number(agent.token_usage?.total_tokens || 0).toLocaleString()
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
  if (value.includes('run') || value.includes('wait') || value.includes('start')) return 'green'
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
