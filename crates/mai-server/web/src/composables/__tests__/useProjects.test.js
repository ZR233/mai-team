import assert from 'node:assert/strict'

import { useProjects } from '../useProjects.js'

const calls = []
let activeReviewerId = 'reviewer-1'

globalThis.requestAnimationFrame = (callback) => {
  callback()
  return 1
}

globalThis.fetch = async (path, init = {}) => {
  calls.push({
    path,
    method: init.method || 'GET',
    body: init.body ? JSON.parse(init.body) : null
  })

  if (path === '/projects') {
    return jsonResponse([
      {
        id: 'project-1',
        name: 'Example Project',
        status: 'ready',
        clone_status: 'ready',
        maintainer_agent_id: 'maintainer-1'
      }
    ])
  }

  if (path === '/projects/project-1') {
    return jsonResponse(projectDetail('maintainer-1'))
  }

  if (path === '/projects/project-1?agent_id=reviewer-1') {
    return jsonResponse(projectDetail(activeReviewerId === 'reviewer-1' ? 'reviewer-1' : 'maintainer-1'))
  }

  if (path === '/projects/project-1?agent_id=reviewer-1&session_id=reviewer-1-session') {
    return jsonResponse(projectDetail(activeReviewerId === 'reviewer-1' ? 'reviewer-1' : 'maintainer-1'))
  }

  if (path === '/projects/project-1?agent_id=reviewer-2') {
    return jsonResponse(projectDetail('reviewer-2'))
  }

  if (path === '/projects/project-1/skills') {
    return jsonResponse({
      roots: [],
      skills: [],
      errors: []
    })
  }

  throw new Error(`Unexpected request: ${path}`)
}

const state = useProjects()

await state.selectProject('project-1')
assert.equal(state.selectedProjectAgentId.value, 'maintainer-1')
assert.equal(state.selectedProjectDetail.value.selected_agent.id, 'maintainer-1')

await state.selectProjectAgent('reviewer-1')
assert.equal(state.selectedProjectAgentId.value, 'reviewer-1')
assert.equal(state.selectedProjectDetail.value.selected_agent.id, 'reviewer-1')

activeReviewerId = 'reviewer-2'
await state.refreshProjectDetail()

assert.equal(state.selectedProjectAgentId.value, 'reviewer-2')
assert.equal(state.selectedProjectSessionId.value, 'reviewer-2-session')
assert.equal(state.selectedProjectDetail.value.selected_agent.id, 'reviewer-2')
assert.equal(
  calls.some((call) => call.path === '/projects/project-1?agent_id=reviewer-1'),
  true
)
assert.equal(
  calls.some((call) => call.path === '/projects/project-1?agent_id=reviewer-2'),
  true
)

function projectDetail(selectedAgentId) {
  const maintainerAgent = {
    id: 'maintainer-1',
    role: 'planner',
    name: 'Maintainer',
    status: 'idle',
    selected_session_id: 'maintainer-session',
    messages: [],
    recent_events: []
  }
  const reviewerAgent = {
    id: activeReviewerId,
    role: 'reviewer',
    name: 'Auto Reviewer',
    status: 'running_turn',
    selected_session_id: `${activeReviewerId}-session`,
    messages: [],
    recent_events: []
  }
  const agents = [maintainerAgent, reviewerAgent]
  const selectedAgent = agents.find((agent) => agent.id === selectedAgentId) || maintainerAgent
  return {
    id: 'project-1',
    name: 'Example Project',
    status: 'ready',
    clone_status: 'ready',
    selected_agent_id: selectedAgent.id,
    maintainer_agent: maintainerAgent,
    agents,
    selected_agent: selectedAgent
  }
}

function jsonResponse(payload) {
  return {
    ok: true,
    status: 200,
    async text() {
      return JSON.stringify(payload)
    }
  }
}
