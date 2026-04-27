import { reactive } from 'vue'
import { useApi } from './useApi'

const providersState = reactive({
  providers: [],
  default_provider_id: null
})

export function useProviders() {
  const { api, showToast } = useApi()

  async function loadProviders() {
    const response = await api('/providers')
    providersState.providers = response.providers || []
    providersState.default_provider_id = response.default_provider_id || null
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
      name: '',
      base_url: '',
      api_key: '',
      default_model: '',
      modelsText: '',
      enabled: true,
      default_provider: false,
      has_api_key: false
    }
  }

  function nextProviderId() {
    let index = providersState.providers.length + 1
    let id = `provider-${index}`
    while (providersState.providers.some((p) => p.id === id)) {
      index += 1
      id = `provider-${index}`
    }
    return id
  }

  function openProviderDialog(index) {
    providerDialog.index = index
    providerDialog.error = ''
    const provider =
      index === null
        ? {
            id: nextProviderId(),
            name: 'New Provider',
            base_url: 'https://api.openai.com/v1',
            default_model: 'gpt-5.2',
            models: ['gpt-5.2'],
            enabled: true,
            has_api_key: false
          }
        : providersState.providers[index]
    providerDialog.form = {
      id: provider.id || '',
      name: provider.name || '',
      base_url: provider.base_url || '',
      api_key: '',
      default_model: provider.default_model || '',
      modelsText: (provider.models || []).join('\n'),
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
    const models = form.modelsText
      .split(/\n|,/)
      .map((value) => value.trim())
      .filter(Boolean)
    if (!form.id || !form.name || !form.base_url || !form.default_model) {
      providerDialog.error = 'Provider ID, display name, base URL, and default model are required.'
      return
    }
    if (!models.includes(form.default_model)) {
      models.unshift(form.default_model)
    }
    const provider = {
      id: form.id,
      name: form.name,
      base_url: form.base_url,
      api_key: form.api_key,
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
    await saveProviders(nextProviders, defaultProviderId)
    providerDialog.open = false
  }

  return {
    providersState,
    providerDialog,
    loadProviders,
    removeProvider,
    openProviderDialog,
    closeProviderDialog,
    saveProviderDialog,
    showToast
  }
}
