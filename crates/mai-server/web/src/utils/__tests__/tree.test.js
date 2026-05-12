import assert from 'node:assert/strict'

import { buildAgentTreeRows, countAgentDescendants } from '../agentTree.js'
import { buildTaskTreeRows } from '../taskTree.js'

const created_at = '2026-05-10T00:00:00.000Z'

function agent(id, parent_id = null) {
  return { id, parent_id, created_at, role: 'explorer' }
}

{
  const rows = buildAgentTreeRows([
    agent('root'),
    agent('self', 'self'),
    agent('cycle-a', 'cycle-b'),
    agent('cycle-b', 'cycle-a'),
    agent('child', 'root')
  ])

  assert.deepEqual(new Set(rows.map((row) => row.agent.id)), new Set(['root', 'child', 'cycle-a', 'cycle-b', 'self']))
  assert.equal(rows.find((row) => row.agent.id === 'root')?.child_count, 1)
  assert.equal(rows.find((row) => row.agent.id === 'cycle-a')?.is_orphan, true)
  assert.equal(rows.find((row) => row.agent.id === 'self')?.is_orphan, true)
  assert.equal(countAgentDescendants('self', [agent('self', 'self')]), 0)
}

{
  const rows = buildTaskTreeRows(
    [{ id: 'task-1', created_at }],
    {
      id: 'task-1',
      agents: [agent('root'), agent('cycle-a', 'cycle-b'), agent('cycle-b', 'cycle-a')]
    }
  )

  assert.deepEqual(rows.map((row) => row.id), ['task-1', 'cycle-a', 'cycle-b', 'root'])
}
