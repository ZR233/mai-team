import assert from 'node:assert/strict'

import {
  defaultProviderApiKeyEnv,
  providerKindForForm
} from '../../utils/providers.js'

assert.equal(defaultProviderApiKeyEnv({ id: 'zhipu', kind: 'zhipu' }), 'ZAI_API_KEY')
assert.equal(defaultProviderApiKeyEnv({ id: 'deepseek', kind: 'deepseek' }), 'DEEPSEEK_API_KEY')
assert.equal(defaultProviderApiKeyEnv({ id: 'mimo-api', kind: 'mimo' }), 'MIMO_API_KEY')
assert.equal(defaultProviderApiKeyEnv({ id: 'mimo-token-plan', kind: 'mimo' }), 'MIMO_TOKEN_PLAN_API_KEY')
assert.equal(defaultProviderApiKeyEnv({ id: 'openai', kind: 'openai' }), 'OPENAI_API_KEY')

assert.equal(providerKindForForm({ kind: 'zhipu' }), 'zhipu')
assert.equal(providerKindForForm({ kind: 'mimo-api' }), 'mimo')
assert.equal(providerKindForForm({ kind: 'mimo-token-plan' }), 'mimo')
