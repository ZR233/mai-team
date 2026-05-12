export function buildAgentTreeRows(agents = []) {
  const sorted = [...agents].sort(compareAgents)
  const byId = new Map(sorted.map((agent) => [agent.id, agent]))
  const childrenByParent = new Map()
  const roots = []

  for (const agent of sorted) {
    const parentId = agent.parent_id || null
    if (canAttachToParent(agent, byId)) {
      const children = childrenByParent.get(parentId) || []
      children.push(agent)
      childrenByParent.set(parentId, children)
    } else {
      roots.push(agent)
    }
  }

  const rows = []
  const visited = new Set()
  const append = (agent, depth, isOrphan) => {
    if (!agent?.id || visited.has(agent.id)) return
    visited.add(agent.id)
    const children = childrenByParent.get(agent.id) || []
    rows.push({
      agent,
      depth,
      child_count: countDescendants(agent.id, childrenByParent),
      is_orphan: isOrphan || hasBrokenParent(agent, byId)
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

function countDescendants(agentId, childrenByParent, visited = new Set([agentId])) {
  const children = childrenByParent.get(agentId) || []
  let count = 0
  for (const child of children) {
    if (!child?.id || visited.has(child.id)) continue
    visited.add(child.id)
    count += 1 + countDescendants(child.id, childrenByParent, visited)
  }
  return count
}

function canAttachToParent(agent, byId) {
  const parentId = agent.parent_id || null
  return Boolean(parentId && byId.has(parentId) && !parentChainHasCycle(agent, byId))
}

function hasBrokenParent(agent, byId) {
  const parentId = agent.parent_id || null
  return Boolean(parentId && (!byId.has(parentId) || parentChainHasCycle(agent, byId)))
}

function parentChainHasCycle(agent, byId) {
  const seen = new Set([agent.id])
  let parentId = agent.parent_id || null
  while (parentId) {
    if (seen.has(parentId)) return true
    seen.add(parentId)
    const parent = byId.get(parentId)
    if (!parent) return false
    parentId = parent.parent_id || null
  }
  return false
}

function compareAgents(left, right) {
  const leftTime = new Date(left.created_at || 0).getTime()
  const rightTime = new Date(right.created_at || 0).getTime()
  if (leftTime !== rightTime) return leftTime - rightTime
  return String(left.id || '').localeCompare(String(right.id || ''))
}
