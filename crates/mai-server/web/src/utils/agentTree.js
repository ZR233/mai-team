export function buildAgentTreeRows(agents = []) {
  const sorted = [...agents].sort(compareAgents)
  const byId = new Map(sorted.map((agent) => [agent.id, agent]))
  const childrenByParent = new Map()
  const roots = []

  for (const agent of sorted) {
    const parentId = agent.parent_id || null
    if (parentId && byId.has(parentId)) {
      const children = childrenByParent.get(parentId) || []
      children.push(agent)
      childrenByParent.set(parentId, children)
    } else {
      roots.push(agent)
    }
  }

  const rows = []
  const append = (agent, depth, isOrphan) => {
    const children = childrenByParent.get(agent.id) || []
    rows.push({
      agent,
      depth,
      child_count: countDescendants(agent.id, childrenByParent),
      is_orphan: isOrphan || Boolean(agent.parent_id && !byId.has(agent.parent_id))
    })
    for (const child of children) {
      append(child, depth + 1, false)
    }
  }

  for (const agent of roots) {
    append(agent, 0, false)
  }

  return rows
}

export function countAgentDescendants(agentId, agents = []) {
  const childrenByParent = new Map()
  for (const agent of agents) {
    if (!agent.parent_id) continue
    const children = childrenByParent.get(agent.parent_id) || []
    children.push(agent)
    childrenByParent.set(agent.parent_id, children)
  }
  return countDescendants(agentId, childrenByParent)
}

function countDescendants(agentId, childrenByParent) {
  const children = childrenByParent.get(agentId) || []
  return children.reduce(
    (count, child) => count + 1 + countDescendants(child.id, childrenByParent),
    0
  )
}

function compareAgents(left, right) {
  const leftTime = new Date(left.created_at || 0).getTime()
  const rightTime = new Date(right.created_at || 0).getTime()
  if (leftTime !== rightTime) return leftTime - rightTime
  return String(left.id || '').localeCompare(String(right.id || ''))
}
