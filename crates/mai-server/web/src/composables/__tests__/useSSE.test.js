import assert from 'node:assert/strict'

import { useSSE, __testOnlyUpdateStreamingEvents } from '../useSSE.js'

const { streamingEvents } = useSSE()

streamingEvents.value = []

__testOnlyUpdateStreamingEvents({
  type: 'agent_message_delta',
  sequence: 1,
  timestamp: '2026-05-10T00:00:00.000Z',
  agent_id: 'agent-1',
  session_id: 'session-1',
  turn_id: 'turn-1',
  message_id: 'message-1',
  role: 'assistant',
  channel: 'final',
  delta: 'hello'
})

assert.equal(streamingEvents.value.length, 1)
assert.equal(streamingEvents.value[0].delta, 'hello')

__testOnlyUpdateStreamingEvents({
  type: 'agent_message_completed',
  sequence: 2,
  timestamp: '2026-05-10T00:00:00.100Z',
  agent_id: 'agent-1',
  session_id: 'session-1',
  turn_id: 'turn-1',
  message_id: 'message-1',
  role: 'assistant',
  channel: 'final',
  content: 'hello'
})

assert.deepEqual(streamingEvents.value, [])

