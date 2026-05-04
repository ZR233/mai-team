import { reactive } from 'vue'
import { useApi } from './useApi'

const providersState = reactive({
  providers: [],
  default_provider_id: null,
  presets: []
})

export function useProviders() {
  const { api, showToast } = useApi()

  async function loadProviders() {
    const [providersResponse, presetsResponse] = await Promise.all([
      api('/providers'),
      api('/provider-presets')
    ])
    providersState.providers = providersResponse.providers || []
    providersState.default_provider_id = providersResponse.default_provider_id || null
    providersState.presets = presetsResponse.providers || []
  }

  async function saveProviders(providers, defaultProviderId) {
    const response = await api('/providers', {
      method: 'PUT',
      body: JSON.stringify({
        providers,
        default_provider_id: defaultProviderId
      })
    })
    providersState.providers = response.providers || []
    providersState.default_provider_id = response.default_provider_id || null
  }

  async function removeProvider(index) {
    const nextProviders = providersState.providers.filter((_, i) => i !== index)
    const defaultProviderId = nextProviders.some((p) => p.id === providersState.default_provider_id)
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
      kind: 'openai',
      name: '',
      base_url: '',
      api_key: '',
      api_key_env: '',
      default_model: '',
      modelsText: '',
      enabled: true,
      default_provider: false,
      has_api_key: false
    }
  }

  function presetFor(kind) {
    return providersState.presets.find((preset) => preset.kind === kind) || providersState.presets[0]
  }

  function fillFromPreset(kind) {
    const preset = presetFor(kind)
    if (!preset) return
    providerDialog.form.kind = preset.kind
    providerDialog.form.id = preset.id
    providerDialog.form.name = preset.name
    providerDialog.form.base_url = preset.base_url
    providerDialog.form.api_key_env = preset.kind === 'openai' ? 'OPENAI_API_KEY' : 'DEEPSEEK_API_KEY'
    providerDialog.form.default_model = preset.default_model
    providerDialog.form.modelsText = JSON.stringify(preset.models || [], null, 2)
  }

  function openProviderDialog(index) {
    providerDialog.index = index
    providerDialog.error = ''
    const preset = presetFor('openai')
    const provider =
      index === null
        ? preset || {
            id: 'openai',
            kind: 'openai',
            name: 'OpenAI',
            base_url: 'https://api.openai.com/v1',
            default_model: 'gpt-5.5',
            models: [],
            enabled: true,
            has_api_key: false
          }
        : providersState.providers[index]
    providerDialog.form = {
      id: provider.id || '',
      kind: provider.kind || 'openai',
      name: provider.name || '',
      base_url: provider.base_url || '',
      api_key: '',
      api_key_env: provider.api_key_env || (provider.kind === 'deepseek' ? 'DEEPSEEK_API_KEY' : 'OPENAI_API_KEY'),
      default_model: provider.default_model || '',
      modelsText: JSON.stringify(provider.models || [], null, 2),
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
    let models = []
    try {
      models = JSON.parse(form.modelsText || '[]')
    } catch {
      providerDialog.error = 'Models must be valid JSON.'
      return
    }
    if (!Array.isArray(models)) {
      providerDialog.error = 'Models must be a JSON array.'
      return
    }
    models = models.map((model) => ({
      ...model,
      id: String(model.id || '').trim(),
      context_tokens: Number(model.context_tokens || 0),
      output_tokens: Number(model.output_tokens || 0),
      supports_tools: model.supports_tools !== false,
      reasoning: normalizeReasoning(model),
      options: model.options || null,
      headers: model.headers || {}
    }))
    if (!form.id || !form.name || !form.base_url || !form.default_model) {
      providerDialog.error = 'Provider ID, display name, base URL, and default model are required.'
      return
    }
    if (!models.length || models.some((model) => !model.id || !model.context_tokens || !model.output_tokens)) {
      providerDialog.error = 'Every model needs id, context_tokens, and output_tokens.'
      return
    }
    if (!models.some((model) => model.id === form.default_model)) {
      providerDialog.error = 'Default model must exist in the model list.'
      return
    }
    const provider = {
      id: form.id,
      kind: form.kind,
      name: form.name,
      base_url: form.base_url,
      api_key: form.api_key,
      api_key_env: form.api_key_env,
      default_model: form.default_model,
      models,
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

function normalizeReasoning(model) {
  if (model.reasoning && Array.isArray(model.reasoning.variants)) {
    return {
      default_variant: model.reasoning.default_variant || null,
      variants: model.reasoning.variants.map((variant) => ({
        ...variant,
        id: String(variant.id || '').trim(),
        label: variant.label || null,
        request: variant.request && typeof variant.request === 'object' && !Array.isArray(variant.request)
          ? variant.request
          : {}
      })).filter((variant) => variant.id)
    }
  }
  const efforts = Array.isArray(model.reasoning_efforts)
    ? model.reasoning_efforts.map((value) => String(value || '').trim()).filter(Boolean)
    : []
  if (!model.supports_reasoning || !efforts.length) return null
  return {
    default_variant: model.default_reasoning_effort || efforts[0],
    variants: efforts.map((id) => ({
      id,
      label: null,
      request: {
        reasoning: {
          effort: id
        }
      }
    }))
  }
}
