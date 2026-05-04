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
  const variants = Array.isArray(model?.reasoning?.variants) ? model.reasoning.variants : []
  if (variants.length) {
    return variants
      .filter((variant) => variant?.id)
      .map((variant) => ({
        value: String(variant.id),
        label: variant.label || reasoningLabel(variant.id)
      }))
      .filter((option, index, items) => items.findIndex((item) => item.value === option.value) === index)
  }
  return []
}

export function defaultReasoningEffort(provider, model) {
  const options = reasoningOptionsFor(provider, model)
  if (!options.length) return ''
  const configured = String(model?.reasoning?.default_variant || '')
  return options.some((option) => option.value === configured)
    ? configured
    : options[0].value
}

export function reasoningLabel(value) {
  if (!value) return ''
  const normalized = String(value)
  return EFFORT_LABELS[normalized.toLowerCase()] || normalized
    .replace(/[_-]+/g, ' ')
    .replace(/\b\w/g, (char) => char.toUpperCase())
}
