import assert from 'node:assert/strict'

import { useEnvironments } from '../useEnvironments.js'

const calls = []
let createdEnvironment = null

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

  if (path === '/environments') {
    if (init.method === 'POST') {
      createdEnvironment = {
        id: 'env-new',
        name: 'Toolchain',
        status: 'idle',
        root_agent_id: 'agent-new',
        conversation_count: 0,
        docker_image: 'ghcr.io/example/toolchain:latest'
      }
      return jsonResponse({ environment: createdEnvironment })
    }
    return jsonResponse([
      {
        id: 'env-1',
        name: 'Default',
        status: 'idle',
        root_agent_id: 'agent-1',
        conversation_count: 1,
        docker_image: 'ubuntu:latest'
      },
      createdEnvironment
    ].filter(Boolean))
  }

  if (path === '/environments:ensure-default') {
    return jsonResponse({ id: 'env-1', name: 'Default', root_agent_id: 'agent-1' })
  }

  if (path === '/environments/env-1') {
    return jsonResponse(environmentDetail('session-1'))
  }

  if (path === '/environments/env-1?session_id=session-2') {
    return jsonResponse(environmentDetail('session-2'))
  }

  if (path === '/environments/env-1/conversations') {
    return jsonResponse({ id: 'session-2', title: 'Second thread' })
  }

  if (path === '/environments/env-1/conversations/session-2/messages') {
    return jsonResponse({ ok: true })
  }

  if (path === '/agents/agent-1') {
    return jsonResponse({ agent: { id: 'agent-1', model: 'gpt-5' } })
  }

  if (path === '/environments/env-new') {
    return jsonResponse({
      ...createdEnvironment,
      selected_conversation_id: null,
      conversations: [],
      root_agent: {
        id: 'agent-new',
        name: 'Toolchain',
        status: 'idle',
        selected_session_id: null,
        sessions: [],
        messages: []
      }
    })
  }

  throw new Error(`Unexpected request: ${path}`)
}

const state = useEnvironments()

await state.refreshEnvironments()
assert.deepEqual(calls.at(-1), { path: '/environments', method: 'GET', body: null })
assert.equal(state.environments.value[0].id, 'env-1')

await state.ensureDefaultEnvironment()
assert.equal(state.selectedEnvironmentId.value, 'env-1')
assert.equal(state.selectedConversationId.value, 'session-1')

await state.createConversation()
assert.equal(state.selectedConversationId.value, 'session-2')
assert.deepEqual(calls.find((call) => call.path === '/environments/env-1/conversations'), {
  path: '/environments/env-1/conversations',
  method: 'POST',
  body: null
})
assert.equal(calls.some((call) => call.path === '/environments/env-1?session_id=session-2'), true)

await state.sendEnvironmentMessage('hello', ['rust'])
assert.deepEqual(calls.at(-3), {
  path: '/environments/env-1/conversations/session-2/messages',
  method: 'POST',
  body: { message: 'hello', skill_mentions: ['rust'] }
})

await state.updateAgent('agent-1', 'openai', 'gpt-5', 'high')
assert.deepEqual(calls.findLast((call) => call.path === '/agents/agent-1'), {
  path: '/agents/agent-1',
  method: 'PATCH',
  body: { provider_id: 'openai', model: 'gpt-5', reasoning_effort: 'high' }
})

await state.createEnvironment('Toolchain', 'ghcr.io/example/toolchain:latest')
assert.equal(state.selectedEnvironmentId.value, 'env-new')

function environmentDetail(selectedConversationId) {
  return {
    id: 'env-1',
    name: 'Default',
    status: 'idle',
    root_agent_id: 'agent-1',
    selected_conversation_id: selectedConversationId,
    conversations: [
      { id: 'session-1', title: 'First thread', message_count: 1 },
      { id: 'session-2', title: 'Second thread', message_count: 0 }
    ],
    root_agent: {
      id: 'agent-1',
      name: 'Default',
      status: 'idle',
      selected_session_id: selectedConversationId,
      sessions: [
        { id: 'session-1', title: 'First thread', message_count: 1 },
        { id: 'session-2', title: 'Second thread', message_count: 0 }
      ],
      messages: []
    }
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
