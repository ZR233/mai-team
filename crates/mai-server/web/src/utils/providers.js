export function defaultProviderApiKeyEnv(provider) {
  if (provider.id === 'mimo-token-plan') return 'MIMO_TOKEN_PLAN_API_KEY'
  if (provider.id === 'mimo-api' || provider.kind === 'mimo') return 'MIMO_API_KEY'
  if (provider.kind === 'zhipu') return 'ZAI_API_KEY'
  if (provider.kind === 'deepseek') return 'DEEPSEEK_API_KEY'
  return 'OPENAI_API_KEY'
}

export function providerKindForForm(form) {
  const presetKey = form.kind
  if (presetKey === 'mimo-api' || presetKey === 'mimo-token-plan') return 'mimo'
  return presetKey
}
