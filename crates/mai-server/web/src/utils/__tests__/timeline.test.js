import assert from 'node:assert/strict'

import { buildAgentTimeline } from '../timeline.js'

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

assert.equal(skillRow?.type, 'process')
assert.equal(skillRow?.tone, 'done')
assert.equal(skillRow?.label, 'Used skills')
assert.equal(skillRow?.detail, 'Demo Skill')
assert.ok(skillRow.sequence < thinkingRow.sequence)

