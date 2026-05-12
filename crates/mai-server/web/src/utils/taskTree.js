export function buildTaskTreeRows(tasks = [], detail = null) {
  const rows = [...tasks].sort(compareTasks).map((task) => ({
    type: 'task',
    task,
    id: task.id,
    depth: 0
  }))

  if (!detail?.id) return rows

  const index = rows.findIndex((row) => row.id === detail.id)
  if (index === -1) return rows

  const agentRows = buildAgentRows(detail.agents || [])
  rows.splice(index + 1, 0, ...agentRows)
  return rows
}

function buildAgentRows(agents = []) {
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
  const append = (agent, depth) => {
    if (!agent?.id || visited.has(agent.id)) return
    visited.add(agent.id)
    rows.push({
      type: 'agent',
      agent,
      id: agent.id,
      depth: depth + 1
    })
    for (const child of childrenByParent.get(agent.id) || []) {
      append(child, depth + 1)
    }
  }

  for (const agent of roots) append(agent, 0)
  return rows
}

function compareTasks(left, right) {
  const leftTime = new Date(left.created_at || 0).getTime()
  const rightTime = new Date(right.created_at || 0).getTime()
  if (leftTime !== rightTime) return leftTime - rightTime
  return String(left.id || '').localeCompare(String(right.id || ''))
}

function compareAgents(left, right) {
  const leftTime = new Date(left.created_at || 0).getTime()
  const rightTime = new Date(right.created_at || 0).getTime()
  if (leftTime !== rightTime) return leftTime - rightTime
  return roleRank(left.role) - roleRank(right.role) || String(left.id || '').localeCompare(String(right.id || ''))
}

function roleRank(role) {
  return {
    planner: 0,
    explorer: 1,
    executor: 2,
    reviewer: 3
  }[String(role || '').toLowerCase()] ?? 9
}

function canAttachToParent(agent, byId) {
  const parentId = agent.parent_id || null
  return Boolean(parentId && byId.has(parentId) && !parentChainHasCycle(agent, byId))
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
