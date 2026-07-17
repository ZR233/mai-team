export function credentialForPreset(preset) {
  return {
    label: preset?.credential?.label || 'API Key',
    envVar: preset?.credential?.env_var || ''
  }
}

export function catalogModelsForPreset(snapshot, preset) {
  if (!snapshot || !preset) return []
  return snapshot.model_catalogs?.[preset.model_catalog_id]?.models || []
}

export function presetForProvider(snapshot, provider) {
  const presets = snapshot?.presets || []
  return presets.find((preset) => preset.id === provider?.preset_id)
    || presets.find((preset) => (
      preset.model_catalog_id === provider?.catalog?.catalog_id
      && normalizeUrl(preset.base_url) === normalizeUrl(provider?.base_url)
    ))
    || null
}

export function uiModelFromDescriptor(model) {
  const reasoning = model?.reasoning
    ? {
        default_variant: model.reasoning.default || null,
        variants: (model.reasoning.candidates || []).map((candidate) => ({
          id: candidate,
          label: null,
          request: {}
        }))
      }
    : null
  return {
    id: model?.id || '',
    name: model?.display_name || model?.id || '',
    context_tokens: model?.context_window || 0,
    max_context_tokens: model?.max_context_window ?? null,
    effective_context_window_percent: 95,
    output_tokens: model?.max_output_tokens || 0,
    supports_tools: model?.capabilities?.function_calling === true,
    capabilities: {
      tools: model?.capabilities?.function_calling === true,
      parallel_tools: model?.capabilities?.parallel_tool_calls === true,
      reasoning_replay: model?.capabilities?.reasoning === true,
      strict_schema: false
    },
    reasoning
  }
}

function normalizeUrl(value) {
  return String(value || '').trim().replace(/\/+$/, '')
}
