import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, resolve } from 'node:path'

import { chatContainerState } from '../chatContainerState.js'

const here = dirname(fileURLToPath(import.meta.url))
const componentPath = resolve(here, '../../components/ChatEnvironmentWorkspace.vue')

assert.deepEqual(
  chatContainerState({
    detail: null,
    loading: false,
    selectedConversationId: null
  }),
  {
    containerReady: false,
    composerDisabled: true,
    disabledReason: 'Preparing the default chat container',
    statusItem: {
      type: 'process',
      key: 'default-container-preparing',
      tone: 'active',
      label: 'Preparing Default Container',
      detail: 'Preparing the default chat container'
    }
  }
)

assert.equal(
  chatContainerState({
    detail: environmentDetail({ status: 'created', container_id: null }),
    selectedConversationId: 'session-1'
  }).disabledReason,
  'Container startup will begin shortly'
)

assert.deepEqual(
  chatContainerState({
    detail: environmentDetail({
      status: 'starting_container',
      container_id: null,
      docker_image: 'ubuntu:latest'
    }),
    selectedConversationId: 'session-1'
  }).statusItem,
  {
    type: 'process',
    key: 'container-starting-agent-1',
    tone: 'active',
    label: 'Starting Container',
    detail: 'Preparing ubuntu:latest'
  }
)

assert.deepEqual(
  chatContainerState({
    detail: environmentDetail({
      status: 'failed',
      container_id: null,
      last_error: 'docker pull failed'
    }),
    selectedConversationId: 'session-1'
  }),
  {
    containerReady: false,
    composerDisabled: true,
    disabledReason: 'docker pull failed',
    statusItem: {
      type: 'process',
      key: 'container-failed-agent-1',
      tone: 'error',
      label: 'Container Failed',
      detail: 'docker pull failed'
    }
  }
)

assert.deepEqual(
  chatContainerState({
    detail: environmentDetail({ status: 'idle', container_id: 'container-1' }),
    selectedConversationId: 'session-1'
  }),
  {
    containerReady: true,
    composerDisabled: false,
    disabledReason: '',
    statusItem: null
  }
)

assert.deepEqual(
  chatContainerState({
    detail: environmentDetail({ status: 'idle', container_id: null }),
    selectedConversationId: 'session-1'
  }),
  {
    containerReady: false,
    composerDisabled: true,
    disabledReason: 'Waiting for environment container',
    statusItem: {
      type: 'process',
      key: 'container-missing-agent-1',
      tone: 'muted',
      label: 'Waiting For Container',
      detail: 'Waiting for environment container'
    }
  }
)

assert.equal(
  readFileSync(componentPath, 'utf8').includes('No environment selected'),
  false
)

function environmentDetail(agentPatch) {
  return {
    id: 'env-1',
    name: '默认环境',
    status: 'planning',
    selected_conversation_id: 'session-1',
    root_agent: {
      id: 'agent-1',
      name: '默认环境',
      status: 'idle',
      container_id: 'container-1',
      docker_image: 'ghcr.io/example/default:latest',
      selected_session_id: 'session-1',
      sessions: [{ id: 'session-1', title: 'Chat 1', message_count: 0 }],
      messages: [],
      ...agentPatch
    },
    conversations: [{ id: 'session-1', title: 'Chat 1', message_count: 0 }]
  }
}
