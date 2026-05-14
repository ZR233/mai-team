import assert from 'node:assert/strict'

import {
  filterModels,
  modelOptionSummary,
  resolveModelSelection
} from '../modelPicker.js'

const providers = [
  {
    id: 'openai',
    name: 'OpenAI',
    default_model: 'gpt-5.5',
    models: [
      {
        id: 'gpt-5.5',
        name: 'GPT-5.5',
        context_tokens: 400000,
        output_tokens: 128000,
        supports_tools: true,
        reasoning: {
          default_variant: 'high',
          variants: [
            { id: 'low', label: 'Low' },
            { id: 'high', label: 'High' }
          ]
        }
      },
      {
        id: 'gpt-5.4-mini',
        name: 'GPT-5.4 Mini',
        context_tokens: 128000,
        output_tokens: 32000,
        supports_tools: true
      }
    ]
  },
  {
    id: 'deepseek',
    name: 'DeepSeek',
    default_model: 'deepseek-chat',
    models: [
      {
        id: 'deepseek-chat',
        name: 'DeepSeek Chat',
        context_tokens: 64000,
        output_tokens: 8192,
        supports_tools: false
      }
    ]
  }
]

assert.deepEqual(
  filterModels(providers[0].models, 'mini').map((model) => model.id),
  ['gpt-5.4-mini']
)
assert.deepEqual(
  filterModels(providers[0].models, 'GPT').map((model) => model.id),
  ['gpt-5.5', 'gpt-5.4-mini']
)
assert.deepEqual(filterModels(providers[0].models, 'missing'), [])

assert.equal(modelOptionSummary(providers[0].models[0]), '400K context · tools · reasoning')
assert.equal(modelOptionSummary(providers[1].models[0]), '64K context')

assert.deepEqual(resolveModelSelection(providers, 'openai', '', ''), {
  providerId: 'openai',
  model: 'gpt-5.5',
  reasoningEffort: 'high'
})

assert.deepEqual(resolveModelSelection(providers, 'deepseek', 'gpt-5.5', 'high'), {
  providerId: 'deepseek',
  model: 'deepseek-chat',
  reasoningEffort: ''
})
