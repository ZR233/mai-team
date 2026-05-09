import { reactive } from 'vue'
import { useApi } from './useApi'

const gitAccountsState = reactive({
  accounts: [],
  default_account_id: null,
  loading: false,
  saving: false,
  verifying: false,
  deleting: false,
  loaded: false
})

export function useGitAccounts() {
  const { api } = useApi()

  async function loadGitAccounts() {
    gitAccountsState.loading = true
    try {
      const response = await api('/git/accounts')
      applyResponse(response)
      return response
    } finally {
      gitAccountsState.loading = false
    }
  }

  async function saveGitAccount(payload) {
    gitAccountsState.saving = true
    try {
      const endpoint = payload?.id ? `/git/accounts/${encodeURIComponent(payload.id)}` : '/git/accounts'
      const response = await api(endpoint, {
        method: payload?.id ? 'PUT' : 'POST',
        body: JSON.stringify(payload)
      })
      await loadGitAccounts()
      return response
    } finally {
      gitAccountsState.saving = false
    }
  }

  async function verifyGitAccount(id) {
    gitAccountsState.verifying = true
    try {
      const response = await api(`/git/accounts/${encodeURIComponent(id)}/verify`, { method: 'POST' })
      await loadGitAccounts()
      return response
    } finally {
      gitAccountsState.verifying = false
    }
  }

  async function deleteGitAccount(id) {
    gitAccountsState.deleting = true
    try {
      const response = await api(`/git/accounts/${encodeURIComponent(id)}`, { method: 'DELETE' })
      applyResponse(response)
      return response
    } finally {
      gitAccountsState.deleting = false
    }
  }

  async function setDefaultGitAccount(id) {
    const response = await api('/git/accounts/default', {
      method: 'PUT',
      body: JSON.stringify({ account_id: id })
    })
    applyResponse(response)
    return response
  }

  async function loadGitAccountRepositories(id) {
    if (!id) return { repositories: [] }
    return api(`/git/accounts/${encodeURIComponent(id)}/repositories`)
  }

  return {
    gitAccountsState,
    loadGitAccounts,
    saveGitAccount,
    verifyGitAccount,
    deleteGitAccount,
    setDefaultGitAccount,
    loadGitAccountRepositories
  }
}

function applyResponse(response) {
  gitAccountsState.accounts = response?.accounts || []
  gitAccountsState.default_account_id = response?.default_account_id || null
  gitAccountsState.loaded = true
}
