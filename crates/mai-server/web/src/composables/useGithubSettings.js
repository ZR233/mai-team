import { reactive } from 'vue'
import { useApi } from './useApi'

const githubSettingsState = reactive({
  has_token: false,
  app_id: '',
  base_url: 'https://api.github.com',
  has_private_key: false,
  app_slug: '',
  app_html_url: '',
  owner_login: '',
  owner_type: '',
  install_url: '',
  loading: false,
  appLoading: false,
  saving: false,
  appSaving: false,
  manifestStarting: false,
  loaded: false
})

export function useGithubSettings() {
  const { api } = useApi()

  async function loadGithubSettings() {
    githubSettingsState.loading = true
    try {
      const [tokenResponse, appResponse] = await Promise.all([
        api('/settings/github'),
        api('/settings/github-app')
      ])
      applyResponse(tokenResponse)
      applyAppResponse(appResponse)
      return tokenResponse
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

  async function loadGithubAppSettings() {
    githubSettingsState.appLoading = true
    try {
      const response = await api('/settings/github-app')
      applyAppResponse(response)
      return response
    } finally {
      githubSettingsState.appLoading = false
    }
  }

  async function saveGithubAppSettings(payload) {
    githubSettingsState.appSaving = true
    try {
      const response = await api('/settings/github-app', {
        method: 'PUT',
        body: JSON.stringify(payload)
      })
      applyAppResponse(response)
      return response
    } finally {
      githubSettingsState.appSaving = false
    }
  }

  async function startGithubAppManifest(payload) {
    githubSettingsState.manifestStarting = true
    try {
      const response = await api('/github/app-manifest/start', {
        method: 'POST',
        body: JSON.stringify(payload)
      })
      submitGithubManifest(response)
      return response
    } finally {
      githubSettingsState.manifestStarting = false
    }
  }

  return {
    githubSettingsState,
    loadGithubSettings,
    saveGithubSettings,
    loadGithubAppSettings,
    saveGithubAppSettings,
    startGithubAppManifest
  }
}

function applyResponse(response) {
  githubSettingsState.has_token = response?.has_token ?? false
  githubSettingsState.loaded = true
}

function applyAppResponse(response) {
  githubSettingsState.app_id = response?.app_id || ''
  githubSettingsState.base_url = response?.base_url || 'https://api.github.com'
  githubSettingsState.has_private_key = response?.has_private_key ?? false
  githubSettingsState.app_slug = response?.app_slug || ''
  githubSettingsState.app_html_url = response?.app_html_url || ''
  githubSettingsState.owner_login = response?.owner_login || ''
  githubSettingsState.owner_type = response?.owner_type || ''
  githubSettingsState.install_url = response?.install_url || ''
  githubSettingsState.loaded = true
}

function submitGithubManifest(response) {
  if (!response?.action_url || !response?.manifest || !response?.state) {
    throw new Error('GitHub App manifest response is incomplete.')
  }
  const form = document.createElement('form')
  form.method = 'POST'
  form.action = response.action_url
  form.style.display = 'none'
  form.appendChild(hiddenInput('manifest', JSON.stringify(response.manifest)))
  form.appendChild(hiddenInput('state', response.state))
  document.body.appendChild(form)
  form.submit()
}

function hiddenInput(name, value) {
  const input = document.createElement('input')
  input.type = 'hidden'
  input.name = name
  input.value = value
  return input
}
