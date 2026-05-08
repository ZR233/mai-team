import { reactive } from 'vue'
import { useApi } from './useApi'

const githubSettingsState = reactive({
  has_token: false,
  loading: false,
  saving: false,
  loaded: false
})

export function useGithubSettings() {
  const { api } = useApi()

  async function loadGithubSettings() {
    githubSettingsState.loading = true
    try {
      const response = await api('/settings/github')
      applyResponse(response)
      return response
    } finally {
      githubSettingsState.loading = false
    }
  }

  async function saveGithubSettings(token) {
    githubSettingsState.saving = true
    try {
      const response = await api('/settings/github', {
        method: 'PUT',
        body: JSON.stringify({ token: token || null })
      })
      applyResponse(response)
      return response
    } finally {
      githubSettingsState.saving = false
    }
  }

  return {
    githubSettingsState,
    loadGithubSettings,
    saveGithubSettings
  }
}

function applyResponse(response) {
  githubSettingsState.has_token = response?.has_token ?? false
  githubSettingsState.loaded = true
}
