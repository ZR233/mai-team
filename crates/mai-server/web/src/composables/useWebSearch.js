import { reactive } from 'vue'
import { useApi } from './useApi'

const webSearchState = reactive({
  config: null,
  roles: {},
  loading: false,
  saving: false,
  error: ''
})

export function useWebSearch() {
  const { api } = useApi()

  async function loadWebSearch() {
    webSearchState.loading = true
    webSearchState.error = ''
    try {
      apply(await api('/settings/web-search'))
    } catch (error) {
      webSearchState.error = error.message || String(error)
      throw error
    } finally {
      webSearchState.loading = false
    }
  }

  async function saveWebSearch(config) {
    webSearchState.saving = true
    webSearchState.error = ''
    try {
      apply(await api('/settings/web-search', {
        method: 'PUT',
        body: JSON.stringify(config)
      }))
    } catch (error) {
      webSearchState.error = error.message || String(error)
      throw error
    } finally {
      webSearchState.saving = false
    }
  }

  return { webSearchState, loadWebSearch, saveWebSearch }
}

function apply(response) {
  webSearchState.config = response?.config || null
  webSearchState.roles = response?.roles || {}
}

