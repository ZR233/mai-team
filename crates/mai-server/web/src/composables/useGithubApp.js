import { reactive } from 'vue'
import { useApi } from './useApi'

const githubAppState = reactive({
  relay: null,
  relaySettings: null,
  app: null,
  installations: [],
  loading: false,
  installing: false,
  loadingInstallations: false,
  error: ''
})

export function useGithubApp() {
  const { api } = useApi()

  async function loadRelayStatus() {
    githubAppState.relay = await api('/relay/status')
    return githubAppState.relay
  }

  async function loadRelaySettings() {
    githubAppState.relaySettings = await api('/settings/relay')
    return githubAppState.relaySettings
  }

  async function loadGithubAppSettings() {
    githubAppState.app = await api('/settings/github-app')
    return githubAppState.app
  }

  async function loadGithubAppContext() {
    githubAppState.loading = true
    githubAppState.error = ''
    try {
      const [relay, relaySettings, app] = await Promise.allSettled([loadRelayStatus(), loadRelaySettings(), loadGithubAppSettings()])
      if (relay.status === 'rejected') githubAppState.relay = { enabled: false, connected: false, message: relay.reason?.message || 'Relay unavailable' }
      if (relaySettings.status === 'rejected') githubAppState.relaySettings = { enabled: false, url: '', has_token: false, node_id: 'mai-server' }
      if (app.status === 'rejected') githubAppState.app = null
      return { relay: githubAppState.relay, app: githubAppState.app }
    } finally {
      githubAppState.loading = false
    }
  }

  async function saveRelaySettings(request) {
    githubAppState.loading = true
    githubAppState.error = ''
    try {
      githubAppState.relaySettings = await api('/settings/relay', {
        method: 'PUT',
        body: JSON.stringify(request)
      })
      await loadRelayStatus()
      return githubAppState.relaySettings
    } finally {
      githubAppState.loading = false
    }
  }

  async function saveGithubAppSettings(request) {
    githubAppState.loading = true
    githubAppState.error = ''
    try {
      githubAppState.app = await api('/settings/github-app', {
        method: 'PUT',
        body: JSON.stringify(request)
      })
      return githubAppState.app
    } finally {
      githubAppState.loading = false
    }
  }

  async function startGithubAppInstallation(origin = window.location.origin, returnHash = '#settings=github-app') {
    githubAppState.installing = true
    try {
      return await api('/github/app-installation/start', {
        method: 'POST',
        body: JSON.stringify({ origin, return_hash: returnHash })
      })
    } finally {
      githubAppState.installing = false
    }
  }

  async function loadInstallations(refresh = false) {
    githubAppState.loadingInstallations = true
    try {
      const response = await api(refresh ? '/github/installations:refresh' : '/github/installations', {
        method: refresh ? 'POST' : 'GET'
      })
      githubAppState.installations = response?.installations || []
      return response
    } finally {
      githubAppState.loadingInstallations = false
    }
  }

  async function loadInstallationRepositories(installationId) {
    if (!installationId) return { repositories: [] }
    return api(`/github/installations/${encodeURIComponent(installationId)}/repositories`)
  }

  async function loadInstallationRepositoryPackages(installationId, repositoryFullName) {
    if (!installationId || !repositoryFullName || !repositoryFullName.includes('/')) {
      return { packages: [], warning: null }
    }
    const [owner, ...repoParts] = repositoryFullName.split('/')
    const repo = repoParts.join('/')
    if (!owner || !repo) return { packages: [], warning: null }
    return api(`/github/installations/${encodeURIComponent(installationId)}/repositories/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}/packages`)
  }

  return {
    githubAppState,
    loadRelayStatus,
    loadRelaySettings,
    loadGithubAppSettings,
    loadGithubAppContext,
    saveRelaySettings,
    saveGithubAppSettings,
    startGithubAppInstallation,
    loadInstallations,
    loadInstallationRepositories,
    loadInstallationRepositoryPackages
  }
}
