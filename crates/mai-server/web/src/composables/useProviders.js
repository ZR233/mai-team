import { reactive } from 'vue'
import { useApi } from './useApi.js'
import {
  catalogModelsForPreset,
  credentialForPreset,
  presetForProvider,
  uiModelFromDescriptor
} from '../utils/providerCatalog.js'

const providersState = reactive({
  providers: [],
  default_provider_id: null,
  catalog: null,
  loading: false,
  error: ''
})

export function useProviders() {
  const { api, showToast } = useApi()

  async function loadProviders() {
    providersState.loading = true
    providersState.error = ''
    try {
      const [catalog, providersResponse] = await Promise.all([
        api('/provider-catalog'),
        api('/providers')
      ])
      providersState.catalog = catalog
      providersState.providers = providersResponse.providers || []
      providersState.default_provider_id = providersResponse.default_provider_id || null
    } catch (error) {
      providersState.catalog = null
      providersState.providers = []
      providersState.default_provider_id = null
      providersState.error = error.message || String(error)
      throw error
    } finally {
      providersState.loading = false
    }
  }

  async function saveProviders(providers, defaultProviderId) {
    const response = await api('/providers', {
      method: 'PUT',
      body: JSON.stringify({ providers, default_provider_id: defaultProviderId })
    })
    providersState.providers = response.providers || []
    providersState.default_provider_id = response.default_provider_id || null
  }

  async function removeProvider(index) {
    const nextProviders = providersState.providers.filter((_, i) => i !== index)
    const defaultProviderId = nextProviders.some((provider) => provider.id === providersState.default_provider_id)
      ? providersState.default_provider_id
      : nextProviders[0]?.id || null
    await saveProviders(nextProviders, defaultProviderId)
  }

  const providerDialog = reactive({
    open: false,
    index: null,
    error: '',
    form: emptyProviderForm()
  })

  function emptyProviderForm() {
    return {
      id: '',
      preset_id: '',
      protocol: 'chat_completions',
      connection_mode: 'http',
      connection_modes: [],
      name: '',
      base_url: '',
      api_key: '',
      api_key_env: '',
      credential_label: 'API Key',
      catalog_source: 'explicit',
      catalog_id: '',
      default_model: '',
      modelsText: '[]',
      effectiveModels: [],
      enabled: true,
      default_provider: false,
      has_api_key: false
    }
  }

  function presetFor(id) {
    return providersState.catalog?.presets?.find((preset) => preset.id === id) || null
  }

  function fillFromPreset(id) {
    const preset = presetFor(id)
    if (!preset) {
      providerDialog.form.preset_id = ''
      providerDialog.form.catalog_source = 'explicit'
      providerDialog.form.catalog_id = ''
      providerDialog.form.modelsText = '[]'
      providerDialog.form.effectiveModels = []
      providerDialog.form.protocol = 'chat_completions'
      providerDialog.form.connection_mode = 'http'
      providerDialog.form.connection_modes = connectionModesForProtocol('chat_completions')
      return
    }
    const credential = credentialForPreset(preset)
    const models = catalogModelsForPreset(providersState.catalog, preset).map(uiModelFromDescriptor)
    Object.assign(providerDialog.form, {
      id: uniqueProviderId(preset.id),
      preset_id: preset.id,
      protocol: preset.transport.protocol,
      connection_mode: preset.transport.default_connection_mode,
      connection_modes: preset.transport.connection_modes || [],
      name: preset.display_name,
      base_url: preset.base_url,
      api_key_env: credential.envVar,
      credential_label: credential.label,
      catalog_source: 'bundled',
      catalog_id: preset.model_catalog_id,
      default_model: preset.suggested_model,
      modelsText: '[]',
      effectiveModels: models
    })
  }

  function openProviderDialog(index) {
    providerDialog.index = index
    providerDialog.error = ''
    if (index === null) {
      providerDialog.form = emptyProviderForm()
      const firstPreset = providersState.catalog?.presets?.[0]
      if (!firstPreset) {
        providerDialog.error = providersState.error || 'Provider catalog is unavailable. Retry loading it first.'
      } else {
        fillFromPreset(firstPreset.id)
      }
      providerDialog.open = true
      return
    }

    const provider = providersState.providers[index]
    const preset = presetForProvider(providersState.catalog, provider)
    const catalog = provider.catalog || { source: 'explicit', models: provider.models || [] }
    const editableModels = catalog.source === 'bundled'
      ? catalog.additional_models || []
      : catalog.models || []
    const credential = credentialForPreset(preset)
    providerDialog.form = {
      id: provider.id || '',
      preset_id: preset?.id || provider.preset_id || '',
      protocol: provider.transport?.protocol || preset?.transport?.protocol || 'chat_completions',
      connection_mode: provider.transport?.connection_mode || preset?.transport?.default_connection_mode || 'http',
      connection_modes: provider.transport?.connection_modes || preset?.transport?.connection_modes || [],
      name: provider.name || '',
      base_url: provider.base_url || '',
      api_key: '',
      api_key_env: provider.api_key_env || credential.envVar,
      credential_label: credential.label,
      catalog_source: catalog.source || 'explicit',
      catalog_id: catalog.catalog_id || '',
      default_model: provider.default_model || '',
      modelsText: JSON.stringify(editableModels, null, 2),
      effectiveModels: provider.models || [],
      enabled: provider.enabled !== false,
      default_provider: providersState.default_provider_id === provider.id,
      has_api_key: provider.has_api_key
    }
    providerDialog.open = true
  }

  function closeProviderDialog() {
    providerDialog.open = false
  }

  async function saveProviderDialog() {
    providerDialog.error = ''
    const form = providerDialog.form
    const editableModels = parseModels(form.modelsText, providerDialog)
    if (!editableModels) return
    if (!form.id || !form.name || !form.base_url || !form.default_model) {
      providerDialog.error = 'Provider ID, display name, base URL, and default model are required.'
      return
    }
    if (editableModels.some((model) => !model.id || !model.context_tokens || !model.output_tokens)) {
      providerDialog.error = 'Every custom model needs id, context_tokens, and output_tokens.'
      return
    }
    const effectiveModels = form.catalog_source === 'bundled'
      ? [...catalogModelsForPreset(providersState.catalog, presetFor(form.preset_id)).map(uiModelFromDescriptor), ...editableModels]
      : editableModels
    if (!effectiveModels.some((model) => model.id === form.default_model)) {
      providerDialog.error = 'Default model must exist in the effective model list.'
      return
    }
    const catalog = form.catalog_source === 'bundled'
      ? { source: 'bundled', catalog_id: form.catalog_id, additional_models: editableModels }
      : { source: 'explicit', models: editableModels }
    const provider = {
      id: form.id,
      preset_id: form.preset_id || null,
      transport: {
        protocol: form.protocol,
        connection_mode: form.connection_mode
      },
      name: form.name,
      base_url: form.base_url,
      api_key: form.api_key,
      api_key_env: form.api_key_env || null,
      catalog,
      default_model: form.default_model,
      enabled: form.enabled
    }
    const nextProviders = [...providersState.providers]
    if (providerDialog.index === null) {
      nextProviders.push(provider)
    } else {
      const existing = nextProviders[providerDialog.index]
      nextProviders[providerDialog.index] = {
        ...existing,
        ...provider,
        has_api_key: Boolean(provider.api_key || existing.has_api_key)
      }
    }
    const defaultProviderId = form.default_provider
      ? provider.id
      : providersState.default_provider_id && nextProviders.some((item) => item.id === providersState.default_provider_id)
        ? providersState.default_provider_id
        : nextProviders[0]?.id || null
    try {
      await saveProviders(nextProviders, defaultProviderId)
      providerDialog.open = false
    } catch (error) {
      providerDialog.error = error.message
    }
  }

  function uniqueProviderId(base) {
    const reserved = new Set(
      providersState.providers
        .filter((_, index) => index !== providerDialog.index)
        .map((provider) => provider.id)
    )
    if (!reserved.has(base)) return base
    let suffix = 2
    while (reserved.has(`${base}-${suffix}`)) suffix += 1
    return `${base}-${suffix}`
  }

  function connectionModesForProtocol(protocol) {
    const modes = new Map()
    for (const preset of providersState.catalog?.presets || []) {
      if (preset.transport?.protocol !== protocol) continue
      for (const mode of preset.transport.connection_modes || []) modes.set(mode.id, mode)
    }
    return [...modes.values()]
  }

  return {
    providersState,
    providerDialog,
    loadProviders,
    removeProvider,
    openProviderDialog,
    closeProviderDialog,
    saveProviderDialog,
    fillFromPreset,
    showToast
  }
}

function parseModels(value, dialog) {
  let models
  try {
    models = JSON.parse(value || '[]')
  } catch {
    dialog.error = 'Models must be valid JSON.'
    return null
  }
  if (!Array.isArray(models)) {
    dialog.error = 'Models must be a JSON array.'
    return null
  }
  return models.map(normalizeModel)
}

function normalizeModel(model) {
  return {
    ...model,
    id: String(model.id || '').trim(),
    context_tokens: Number(model.context_tokens || 0),
    max_context_tokens: model.max_context_tokens == null ? null : Number(model.max_context_tokens),
    effective_context_window_percent: Number(model.effective_context_window_percent || 95),
    output_tokens: Number(model.output_tokens || 0),
    supports_tools: model.supports_tools !== false,
    reasoning: normalizeReasoning(model),
    options: model.options || null,
    headers: model.headers || {}
  }
}

function normalizeReasoning(model) {
  if (!model.reasoning || !Array.isArray(model.reasoning.variants)) return null
  return {
    default_variant: model.reasoning.default_variant || null,
    variants: model.reasoning.variants
      .map((variant) => ({
        ...variant,
        id: String(variant.id || '').trim(),
        label: variant.label || null,
        request: variant.request && typeof variant.request === 'object' && !Array.isArray(variant.request)
          ? variant.request
          : {}
      }))
      .filter((variant) => variant.id)
  }
}
