const READY_STATUSES = new Set(['idle', 'completed', 'cancelled'])

export function chatContainerState({
  detail = null,
  loading = false,
  selectedConversationId = null,
  sending = false
} = {}) {
  const agent = detail?.root_agent || null
  if (!agent) {
    const detailText = loading
      ? 'Loading the default chat container'
      : 'Preparing the default chat container'
    return disabledState(detailText, {
      type: 'process',
      key: 'default-container-preparing',
      tone: 'active',
      label: 'Preparing Default Container',
      detail: detailText
    })
  }

  const status = String(agent.status || '').toLowerCase()
  const agentId = agent.id || 'default'

  if (status === 'starting_container') {
    const detailText = agent.docker_image ? `Preparing ${agent.docker_image}` : 'Preparing environment container'
    return disabledState(detailText, {
      type: 'process',
      key: `container-starting-${agentId}`,
      tone: 'active',
      label: 'Starting Container',
      detail: detailText
    })
  }

  if (status === 'created') {
    const detailText = 'Container startup will begin shortly'
    return disabledState(detailText, {
      type: 'process',
      key: `container-created-${agentId}`,
      tone: 'muted',
      label: 'Preparing Environment',
      detail: detailText
    })
  }

  if (status === 'failed') {
    const detailText = agent.last_error || 'Environment container failed to start'
    return disabledState(detailText, {
      type: 'process',
      key: `container-failed-${agentId}`,
      tone: 'error',
      label: 'Container Failed',
      detail: detailText
    })
  }

  if (status === 'deleting_container' || status === 'deleted') {
    const detailText = 'Environment container is unavailable'
    return disabledState(detailText, {
      type: 'process',
      key: `container-unavailable-${agentId}`,
      tone: 'error',
      label: 'Container Unavailable',
      detail: detailText
    })
  }

  if (!READY_STATUSES.has(status)) {
    const detailText = 'Agent is busy'
    return {
      containerReady: false,
      composerDisabled: true,
      disabledReason: detailText,
      statusItem: null
    }
  }

  if (!agent.container_id) {
    const detailText = 'Waiting for environment container'
    return disabledState(detailText, {
      type: 'process',
      key: `container-missing-${agentId}`,
      tone: 'muted',
      label: 'Waiting For Container',
      detail: detailText
    })
  }

  if (!selectedConversationId) {
    return {
      containerReady: true,
      composerDisabled: true,
      disabledReason: 'Preparing the chat conversation',
      statusItem: null
    }
  }

  return {
    containerReady: true,
    composerDisabled: Boolean(sending),
    disabledReason: '',
    statusItem: null
  }
}

function disabledState(reason, statusItem) {
  return {
    containerReady: false,
    composerDisabled: true,
    disabledReason: reason,
    statusItem
  }
}
