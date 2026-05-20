import assert from 'node:assert/strict'

import {
  projectReviewRunOutcomeLabel,
  projectReviewRunOutcomeTone,
  sessionTokenUsage,
  tokenCacheTooltipLabel,
  tokenCacheTooltipRows,
  totalTokens
} from '../format.js'

const detail = {
  token_usage: {
    input_tokens: 999,
    cached_input_tokens: 333,
    output_tokens: 222,
    reasoning_output_tokens: 111,
    total_tokens: 1221
  },
  selected_session_id: 'session-2',
  sessions: [
    {
      id: 'session-1',
      token_usage: {
        input_tokens: 100,
        cached_input_tokens: 20,
        output_tokens: 10,
        reasoning_output_tokens: 4,
        total_tokens: 110
      }
    },
    {
      id: 'session-2',
      token_usage: {
        input_tokens: 200,
        cached_input_tokens: 80,
        output_tokens: 30,
        reasoning_output_tokens: 12,
        total_tokens: 230
      }
    }
  ]
}

assert.deepEqual(sessionTokenUsage(detail), {
  input_tokens: 200,
  cached_input_tokens: 80,
  output_tokens: 30,
  reasoning_output_tokens: 12,
  total_tokens: 230
})

assert.equal(totalTokens(detail), '230')

assert.deepEqual(tokenCacheTooltipRows(detail), [
  { label: 'Input', value: '200' },
  { label: 'Cache hit', value: '80' },
  { label: 'Cache miss', value: '120' },
  { label: 'Hit rate', value: '40%' },
  { label: 'Output', value: '30' },
  { label: 'Reasoning', value: '12' },
  { label: 'Total', value: '230' }
])

assert.equal(
  tokenCacheTooltipLabel(detail),
  'Input 200, Cache hit 80, Cache miss 120, Hit rate 40%, Output 30, Reasoning 12, Total 230'
)

assert.deepEqual(
  tokenCacheTooltipRows({
    selected_session_id: 'empty',
    sessions: [{ id: 'empty', token_usage: {} }]
  }),
  [
    { label: 'Input', value: '0' },
    { label: 'Cache hit', value: '0' },
    { label: 'Cache miss', value: '0' },
    { label: 'Hit rate', value: '0%' },
    { label: 'Output', value: '0' },
    { label: 'Reasoning', value: '0' },
    { label: 'Total', value: '0' }
  ]
)

assert.equal(projectReviewRunOutcomeLabel({ outcome: 'review_submitted', review_event: 'approve' }), 'Approved')
assert.equal(
  projectReviewRunOutcomeLabel({ outcome: 'review_submitted', review_event: 'request_changes' }),
  'Changes Requested'
)
assert.equal(projectReviewRunOutcomeLabel({ outcome: 'review_submitted', review_event: 'comment' }), 'Commented')
assert.equal(projectReviewRunOutcomeLabel({ outcome: 'review_submitted' }), 'Review Submitted')
assert.equal(projectReviewRunOutcomeTone({ review_event: 'approve' }), 'approve')
assert.equal(projectReviewRunOutcomeTone({ review_event: 'request_changes' }), 'request-changes')
assert.equal(projectReviewRunOutcomeTone({ review_event: 'comment' }), 'comment')
