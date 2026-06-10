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
  const contextTokens = effectiveContextTokens(model)
  if (contextTokens) parts.push(`${formatCompactNumber(contextTokens)} context`)
  if (model?.supports_tools) parts.push('tools')
  if (reasoningOptionsFor(null, model).length) parts.push('reasoning')
  return parts.join(' · ')
}

export function effectiveContextTokens(model) {
  const contextTokens = positiveNumber(model?.context_tokens)
  if (contextTokens) {
    const effectiveContextWindowPercent = positiveNumber(model?.effective_context_window_percent)
    if (effectiveContextWindowPercent) {
      return Math.floor(
        contextTokens * Math.min(effectiveContextWindowPercent, 100) / 100
      )
    }
    return contextTokens
  }

  const maxContextTokens = positiveNumber(model?.max_context_tokens)
  if (maxContextTokens) {
    const effectiveContextWindowPercent = positiveNumber(model?.effective_context_window_percent)
    return effectiveContextWindowPercent
      ? Math.floor(
        maxContextTokens * Math.min(effectiveContextWindowPercent, 100) / 100
      )
      : maxContextTokens
  }

  return 0
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

function positiveNumber(value) {
  const number = Number(value || 0)
  return Number.isFinite(number) && number > 0 ? number : 0
}

function trimNumber(value) {
  return value >= 10 ? String(Math.round(value)) : value.toFixed(1).replace(/\.0$/, '')
}
