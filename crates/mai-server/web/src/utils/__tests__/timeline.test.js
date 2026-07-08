import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { buildAgentTimeline, renderToolTrace, timelineItemClasses } from '../timeline.js'

const timelineSource = readFileSync(
  resolve(dirname(fileURLToPath(import.meta.url)), '../timeline.js'),
  'utf8'
)

const detail = {
  id: 'agent-1',
  selected_session_id: 'session-1',
  messages: [],
  recent_events: [
    {
      type: 'turn_started',
      sequence: 1,
      timestamp: '2026-05-10T00:00:00.000Z',
      agent_id: 'agent-1',
      session_id: 'session-1',
      turn_id: 'turn-1'
    },
    {
      type: 'agent_message',
      sequence: 2,
      timestamp: '2026-05-10T00:00:01.000Z',
      agent_id: 'agent-1',
      session_id: 'session-1',
      turn_id: 'turn-1',
      role: 'user',
      content: 'please use $demo'
    },
    {
      type: 'skills_activated',
      sequence: 3,
      timestamp: '2026-05-10T00:00:02.000Z',
      agent_id: 'agent-1',
      session_id: 'session-1',
      turn_id: 'turn-1',
      skills: [
        {
          name: 'demo',
          display_name: 'Demo Skill',
          path: '/workspace/.agents/skills/demo/SKILL.md',
          scope: 'repo'
        }
      ]
    }
  ]
}

const timeline = buildAgentTimeline(detail)
const skillRow = timeline.find((item) => item.key.startsWith('skills-activated-'))
const thinkingRow = timeline.find((item) => item.key.startsWith('thinking-'))

assert.equal(skillRow?.type, 'skill_call')
assert.equal(skillRow?.tone, 'skill')
assert.equal(skillRow?.title, 'Skills activated')
assert.equal(skillRow?.summary, '1 skill loaded')
assert.deepEqual(skillRow?.skills.map((skill) => skill.name), ['Demo Skill'])
assert.ok(skillRow.sequence < thinkingRow.sequence)

assert.deepEqual(
  timelineItemClasses({ type: 'message', role: 'assistant' }),
  ['timeline-message-item', 'timeline-role-assistant']
)
assert.deepEqual(
  timelineItemClasses({ type: 'tool_call', status: 'failed', tone: 'error' }),
  ['timeline-tool_call-item', 'timeline-status-failed', 'timeline-tone-error']
)

const streamedTimeline = buildAgentTimeline({
  id: 'agent-1',
  selected_session_id: 'session-1',
  messages: [],
  recent_events: [
    {
      type: 'agent_message_delta',
      sequence: 1,
      timestamp: '2026-05-10T00:00:00.000Z',
      agent_id: 'agent-1',
      session_id: 'session-1',
      turn_id: 'turn-1',
      message_id: 'message-1',
      role: 'assistant',
      channel: 'final',
      delta: 'hello '
    },
    {
      type: 'agent_message_delta',
      sequence: 2,
      timestamp: '2026-05-10T00:00:00.050Z',
      agent_id: 'agent-1',
      session_id: 'session-1',
      turn_id: 'turn-1',
      message_id: 'message-1',
      role: 'assistant',
      channel: 'final',
      delta: 'world'
    },
    {
      type: 'agent_message_completed',
      sequence: 3,
      timestamp: '2026-05-10T00:00:00.100Z',
      agent_id: 'agent-1',
      session_id: 'session-1',
      turn_id: 'turn-1',
      message_id: 'message-1',
      role: 'assistant',
      channel: 'final',
      content: 'hello world'
    }
  ]
})
const streamedMessage = streamedTimeline.find((item) => item.type === 'message')

assert.equal(streamedMessage?.content, 'hello world')
assert.equal(streamedMessage?.streaming, false)

const completedStreamWithPersistedMessageTimeline = buildAgentTimeline({
  id: 'agent-1',
  selected_session_id: 'session-1',
  messages: [
    {
      role: 'assistant',
      content: 'Now let me fetch and inspect the code locally.',
      created_at: '2026-05-10T00:00:02.000Z'
    }
  ],
  recent_events: [
    {
      type: 'agent_message_completed',
      sequence: 1,
      timestamp: '2026-05-10T00:00:01.000Z',
      agent_id: 'agent-1',
      session_id: 'session-1',
      turn_id: 'turn-1',
      message_id: 'message-1',
      role: 'assistant',
      channel: 'final',
      content: 'Now let me fetch and inspect the code locally.'
    }
  ]
})
const duplicateAssistantMessages = completedStreamWithPersistedMessageTimeline.filter(
  (item) => item.type === 'message' && item.content === 'Now let me fetch and inspect the code locally.'
)

assert.equal(duplicateAssistantMessages.length, 1)
assert.equal(duplicateAssistantMessages[0]?.streaming, false)

const liveStreamingTimeline = buildAgentTimeline(
  {
    id: 'agent-1',
    selected_session_id: 'session-1',
    messages: [],
    recent_events: []
  },
  [
    {
      type: 'agent_message_delta',
      sequence: 2,
      timestamp: '2026-05-10T00:00:00.050Z',
      agent_id: 'agent-1',
      session_id: 'session-1',
      turn_id: 'turn-1',
      message_id: 'message-2',
      role: 'assistant',
      channel: 'final',
      delta: 'hello world'
    },
    {
      type: 'agent_message_delta',
      sequence: 2,
      timestamp: '2026-05-10T00:00:00.050Z',
      agent_id: 'agent-1',
      session_id: 'session-1',
      turn_id: 'turn-1',
      message_id: 'message-2',
      role: 'assistant',
      channel: 'final',
      delta: 'world'
    },
    {
      type: 'agent_message_delta',
      sequence: 1,
      timestamp: '2026-05-10T00:00:00.000Z',
      agent_id: 'agent-1',
      session_id: 'session-1',
      turn_id: 'turn-1',
      message_id: 'message-2',
      role: 'assistant',
      channel: 'final',
      delta: 'hello '
    }
  ]
)
const liveStreamingMessage = liveStreamingTimeline.find((item) => item.type === 'message')

assert.equal(liveStreamingMessage?.content, 'hello world')
assert.equal(liveStreamingMessage?.streaming, true)

const contextTimeline = buildAgentTimeline({
  id: 'agent-1',
  selected_session_id: 'session-1',
  messages: [],
  recent_events: [
    {
      type: 'context_compacted',
      sequence: 1,
      timestamp: '2026-05-10T00:00:03.000Z',
      agent_id: 'agent-1',
      session_id: 'session-1',
      turn_id: 'turn-1',
      tokens_before: 42000
    }
  ]
})
const contextRow = contextTimeline.find((item) => item.key.startsWith('context-compacted-'))

assert.equal(contextRow?.type, 'context_event')
assert.equal(contextRow?.title, 'Context compacted')
assert.equal(contextRow?.summary, '42000 tokens before compaction')

const toolDetail = {
  id: 'agent-1',
  selected_session_id: 'session-1',
  messages: [],
  recent_events: [
    {
      type: 'tool_started',
      sequence: 1,
      timestamp: '2026-05-10T00:00:00.000Z',
      agent_id: 'agent-1',
      session_id: 'session-1',
      turn_id: 'turn-1',
      call_id: 'call-1',
      tool_name: 'container_exec',
      arguments: {
        command: 'rg -n timeline crates/mai-server/web/src',
        cwd: '/workspace'
      }
    },
    {
      type: 'tool_completed',
      sequence: 2,
      timestamp: '2026-05-10T00:00:00.128Z',
      agent_id: 'agent-1',
      session_id: 'session-1',
      turn_id: 'turn-1',
      call_id: 'call-1',
      tool_name: 'container_exec',
      success: true,
      output_preview: JSON.stringify({ status: 0, stdout: 'timeline.js\nToolCallRow.vue\n', stderr: '' }),
      duration_ms: 128
    }
  ]
}

const toolTimeline = buildAgentTimeline(toolDetail)
const toolRow = toolTimeline.find((item) => item.key === 'tool-call-1')

assert.equal(toolRow?.type, 'tool_call')
assert.equal(toolRow?.status, 'completed')
assert.equal(toolRow?.toolActionLabel, 'Ran')
assert.equal(toolRow?.toolPrimary, 'rg -n timeline crates/mai-server/web/src')
assert.equal(toolRow?.durationMs, 128)
assert.deepEqual(toolRow?.toolPreviewLines, ['timeline.js', 'ToolCallRow.vue'])

const execTrace = renderToolTrace({
  toolName: 'container_exec',
  kind: 'output',
  value: JSON.stringify({
    status: 1,
    stdout: '<script>alert("x")</script>',
    stderr: '\u001b[31mfailed\u001b[0m'
  })
})

assert.match(execTrace, /trace-status-error/)
assert.match(execTrace, /&lt;script&gt;alert/)
assert.doesNotMatch(execTrace, /<script>alert/)
assert.match(execTrace, /failed/)

const unknownTrace = renderToolTrace({
  toolName: 'custom_tool',
  kind: 'arguments',
  value: {
    alpha: 'one',
    beta: '<b>two</b>',
    nested: { hidden: true },
    list: [1, 2, 3]
  }
})

assert.match(unknownTrace, /trace-meta-line/)
assert.match(unknownTrace, /alpha/)
assert.match(unknownTrace, /&lt;b&gt;two&lt;\/b&gt;/)
assert.doesNotMatch(unknownTrace, /"nested"/)
assert.doesNotMatch(unknownTrace, /trace-code-json/)

const containerCopyTrace = renderToolTrace({
  toolName: 'container_copy',
  kind: 'arguments',
  value: {
    direction: 'upload',
    path: '/workspace/result.txt',
    contentBase64: 'Zm9vYmFyYmF6'
  }
})

assert.match(containerCopyTrace, /direction/)
assert.match(containerCopyTrace, /upload/)
assert.match(containerCopyTrace, /\/workspace\/result\.txt/)
assert.doesNotMatch(containerCopyTrace, /content_base64/)

const sendInputTrace = renderToolTrace({
  toolName: 'send_input',
  kind: 'arguments',
  value: {
    target: 'agent-2',
    triggerTurn: true,
    message: 'continue'
  }
})

assert.match(sendInputTrace, /agent-2/)
assert.match(sendInputTrace, /trigger/)
assert.doesNotMatch(sendInputTrace, /agent_id/)

for (const legacy of [
  'container_cp_upload',
  'container_cp_download',
  'send_message',
  'content_base64',
  'timeout_secs',
  'provider_id'
]) {
  assert.equal(
    timelineSource.includes(legacy),
    false,
    `timeline rendering must not keep legacy shared tool protocol ${legacy}`
  )
}
