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

class FakeEventSource {
  static instances = []

  constructor(url) {
    this.url = url
    this.listeners = new Map()
    this.closed = false
    FakeEventSource.instances.push(this)
  }

  addEventListener(name, handler) {
    this.listeners.set(name, handler)
  }

  close() {
    this.closed = true
  }

  dispatch(name, data) {
    const handler = this.listeners.get(name)
    if (handler) handler({ data: JSON.stringify(data) })
  }
}

const originalEventSource = globalThis.EventSource
globalThis.EventSource = FakeEventSource

const unhandledRejections = []
const onUnhandledRejection = (reason) => {
  unhandledRejections.push(reason)
}
process.on('unhandledRejection', onUnhandledRejection)

const reportedErrors = []
const { eventFeed, connectEvents, disconnect } = useSSE()
eventFeed.value = []

connectEvents(
  async () => {
    throw new Error('detail refresh failed')
  },
  null,
  (error) => {
    reportedErrors.push(error)
  }
)

FakeEventSource.instances.at(-1).dispatch('agent_updated', {
  type: 'agent_updated',
  sequence: 3,
  timestamp: '2026-05-10T00:00:00.200Z',
  agent: { id: 'agent-1' }
})

await new Promise((resolve) => setTimeout(resolve, 0))

assert.equal(unhandledRejections.length, 0)
assert.equal(reportedErrors.length, 1)
assert.equal(reportedErrors[0].message, 'detail refresh failed')
assert.equal(eventFeed.value.length, 1)

disconnect()
process.off('unhandledRejection', onUnhandledRejection)
globalThis.EventSource = originalEventSource
