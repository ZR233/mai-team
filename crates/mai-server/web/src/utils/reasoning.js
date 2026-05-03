const EFFORT_LABELS = {
  none: 'None',
  minimal: 'Minimal',
  low: 'Low',
  medium: 'Medium',
  high: 'High',
  xhigh: 'Xhigh',
  max: 'Max'
}

export function reasoningOptionsFor(provider, model) {
  if (!model?.supports_reasoning) return []
  const configuredEfforts = Array.isArray(model.reasoning_efforts) ? model.reasoning_efforts : []
  const efforts = configuredEfforts.length
    ? configuredEfforts
    : provider?.kind === 'deepseek' ? ['high', 'max'] : []
  return efforts
    .filter(Boolean)
    .map((value) => String(value).toLowerCase())
    .filter((value, index, items) => items.indexOf(value) === index)
    .map((value) => ({ value, label: EFFORT_LABELS[value] || value }))
}

export function defaultReasoningEffort(provider, model) {
  const options = reasoningOptionsFor(provider, model)
  if (!options.length) return ''
  const configured = String(model?.default_reasoning_effort || '').toLowerCase()
  return options.some((option) => option.value === configured)
    ? configured
    : options[0].value
}

export function reasoningLabel(value) {
  if (!value) return ''
  const normalized = String(value).toLowerCase()
  return EFFORT_LABELS[normalized] || normalized
}
