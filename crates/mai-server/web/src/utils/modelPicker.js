import { defaultReasoningEffort, reasoningOptionsFor } from './reasoning.js'

export function filterModels(models, query) {
  const needle = String(query || '').trim().toLowerCase()
  if (!needle) return models || []
  return (models || []).filter((model) =>
    [model.id, model.name]
      .filter(Boolean)
      .some((value) => String(value).toLowerCase().includes(needle))
  )
}

export function modelOptionSummary(model) {
  const parts = []
  const contextTokens = Number(model?.context_tokens || 0)
  if (contextTokens) parts.push(`${formatCompactNumber(contextTokens)} context`)
  if (model?.supports_tools) parts.push('tools')
  if (reasoningOptionsFor(null, model).length) parts.push('reasoning')
  return parts.join(' · ')
}

export function resolveModelSelection(providers, providerId, model, reasoningEffort) {
  const provider = (providers || []).find((item) => item.id === providerId) || providers?.[0]
  if (!provider) {
    return { providerId: '', model: '', reasoningEffort: '' }
  }

  const models = provider.models || []
  const selectedModel = models.find((item) => item.id === model)
    || models.find((item) => item.id === provider.default_model)
    || models[0]
  const selectedModelId = selectedModel?.id || ''
  const reasoningOptions = reasoningOptionsFor(provider, selectedModel)
  const nextReasoningEffort = reasoningOptions.some((option) => option.value === reasoningEffort)
    ? reasoningEffort
    : defaultReasoningEffort(provider, selectedModel)

  return {
    providerId: provider.id,
    model: selectedModelId,
    reasoningEffort: nextReasoningEffort
  }
}

function formatCompactNumber(value) {
  if (value >= 1_000_000) return `${trimNumber(value / 1_000_000)}M`
  if (value >= 1_000) return `${trimNumber(value / 1_000)}K`
  return String(Math.round(value))
}

function trimNumber(value) {
  return value >= 10 ? String(Math.round(value)) : value.toFixed(1).replace(/\.0$/, '')
}
