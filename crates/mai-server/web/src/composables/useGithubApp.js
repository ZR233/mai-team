import { reactive } from 'vue'
import { useApi } from './useApi'

const githubAppState = reactive({
  relay: null,
  relaySettings: null,
  relayUpdate: null,
  app: null,
  installations: [],
  loading: false,
  checkingRelayUpdate: false,
  updatingRelay: false,
  rollingBackRelay: false,
  installing: false,
  loadingInstallations: false,
  relayUpdateError: '',
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

  async function loadRelayUpdateStatus() {
    githubAppState.relayUpdate = await api('/relay/update')
    githubAppState.relayUpdateError = githubAppState.relayUpdate?.warning || ''
    return githubAppState.relayUpdate
  }

  async function loadGithubAppSettings() {
    githubAppState.app = await api('/settings/github-app')
    return githubAppState.app
  }

  async function loadGithubAppContext() {
    githubAppState.loading = true
    githubAppState.error = ''
    try {
      const [relay, relaySettings, app, relayUpdate] = await Promise.allSettled([loadRelayStatus(), loadRelaySettings(), loadGithubAppSettings(), loadRelayUpdateStatus()])
      if (relay.status === 'rejected') githubAppState.relay = { enabled: false, connected: false, message: relay.reason?.message || 'Relay unavailable' }
      if (relaySettings.status === 'rejected') githubAppState.relaySettings = { enabled: false, url: '', has_token: false, node_id: 'mai-server' }
      if (app.status === 'rejected') githubAppState.app = null
      if (relayUpdate.status === 'rejected') {
        githubAppState.relayUpdate = null
        githubAppState.relayUpdateError = relayUpdate.reason?.message || ''
      }
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
      if (githubAppState.relay?.connected) await loadRelayUpdateStatus()
      return githubAppState.relaySettings
    } finally {
      githubAppState.loading = false
    }
  }

  async function checkRelayUpdate(force = true) {
    githubAppState.checkingRelayUpdate = true
    githubAppState.relayUpdateError = ''
    try {
      githubAppState.relayUpdate = await api('/relay/update:check', {
        method: 'POST',
        body: JSON.stringify({ force })
      })
      githubAppState.relayUpdateError = githubAppState.relayUpdate?.warning || ''
      return githubAppState.relayUpdate
    } catch (error) {
      githubAppState.relayUpdateError = error.message
      throw error
    } finally {
      githubAppState.checkingRelayUpdate = false
    }
  }

  async function applyRelayUpdate() {
    githubAppState.updatingRelay = true
    githubAppState.relayUpdateError = ''
    try {
      const response = await api('/relay/update:apply', { method: 'POST' })
      githubAppState.relayUpdate = response?.status || githubAppState.relayUpdate
      return response
    } catch (error) {
      githubAppState.relayUpdateError = error.message
      throw error
    } finally {
      githubAppState.updatingRelay = false
    }
  }

  async function rollbackRelayUpdate() {
    githubAppState.rollingBackRelay = true
    githubAppState.relayUpdateError = ''
    try {
      const response = await api('/relay/update:rollback', { method: 'POST' })
      githubAppState.relayUpdate = response?.status || githubAppState.relayUpdate
      return response
    } catch (error) {
      githubAppState.relayUpdateError = error.message
      throw error
    } finally {
      githubAppState.rollingBackRelay = false
    }
  }

  async function waitForRelayReconnect(timeoutMs = 30000) {
    const deadline = Date.now() + timeoutMs
    const startedAt = Date.now()
    let sawDisconnect = false
    while (Date.now() < deadline) {
      try {
        const status = await loadRelayStatus()
        if (status?.connected && (sawDisconnect || Date.now() - startedAt > 2500)) return status
        if (!status?.connected) sawDisconnect = true
      } catch (_error) {
        sawDisconnect = true
      }
      await new Promise((resolve) => setTimeout(resolve, 1000))
    }
    throw new Error('Timed out waiting for mai-relay to reconnect.')
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
    loadRelayUpdateStatus,
    loadGithubAppSettings,
    loadGithubAppContext,
    saveRelaySettings,
    checkRelayUpdate,
    applyRelayUpdate,
    rollbackRelayUpdate,
    waitForRelayReconnect,
    saveGithubAppSettings,
    startGithubAppInstallation,
    loadInstallations,
    loadInstallationRepositories,
    loadInstallationRepositoryPackages
  }
}
