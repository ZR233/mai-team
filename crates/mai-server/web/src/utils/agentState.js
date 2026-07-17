const BUSY_ACTIVITIES = new Set(['queued', 'running', 'waiting_tool', 'waiting_interaction'])

export function activeAgentTurn(agent) {
  return agent?.state?.runtime?.active_turn || null
}

export function agentResourceState(agent) {
  return agent?.state?.resource || 'provisioning'
}

export function agentRuntimeActivity(agent) {
  return agent?.state?.runtime?.activity || 'idle'
}

export function agentResourceError(agent) {
  return agent?.state?.resource_error || ''
}

export function isAgentBusy(agent) {
  return agentResourceState(agent) === 'provisioning' || BUSY_ACTIVITIES.has(agentRuntimeActivity(agent))
}

export function canStopAgentTurn(agent) {
  return Boolean(activeAgentTurn(agent)) && BUSY_ACTIVITIES.has(agentRuntimeActivity(agent))
}

export function agentDisplayStatus(agent) {
  const resource = agentResourceState(agent)
  if (resource !== 'ready') return resource
  const lifecycle = agent?.state?.runtime?.lifecycle || 'active'
  if (lifecycle !== 'active') return lifecycle
  return agentRuntimeActivity(agent)
}
