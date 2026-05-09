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

const GIT_ACCOUNT_VERIFY_POLL_INTERVAL_MS = 1000
const GIT_ACCOUNT_VERIFY_POLL_ATTEMPTS = 12

export function useGitAccounts() {
  const { api } = useApi()

  async function loadGitAccounts(options = {}) {
    const silent = options?.silent === true
    if (!silent) gitAccountsState.loading = true
    try {
      const response = await api('/git/accounts', { timeoutMs: 15000 })
      applyResponse(response)
      return response
    } finally {
      if (!silent) gitAccountsState.loading = false
    }
  }

  async function saveGitAccount(payload) {
    gitAccountsState.saving = true
    try {
      const endpoint = payload?.id ? `/git/accounts/${encodeURIComponent(payload.id)}` : '/git/accounts'
      const response = await api(endpoint, {
        method: payload?.id ? 'PUT' : 'POST',
        body: JSON.stringify(payload),
        timeoutMs: 15000
      })
      const account = response?.account || response
      if (account?.id) {
        upsertGitAccount(account)
        loadGitAccounts({ silent: true }).catch(() => {})
        pollGitAccountVerification(account.id).catch(() => {})
      }
      return response
    } finally {
      gitAccountsState.saving = false
    }
  }

  async function verifyGitAccount(id) {
    gitAccountsState.verifying = true
    try {
      const response = await api(`/git/accounts/${encodeURIComponent(id)}/verify`, {
        method: 'POST',
        timeoutMs: 15000
      })
      const account = response?.account || response
      if (account?.id) upsertGitAccount(account)
      loadGitAccounts({ silent: true }).catch(() => {})
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

  async function pollGitAccountVerification(id) {
    for (let attempt = 0; attempt < GIT_ACCOUNT_VERIFY_POLL_ATTEMPTS; attempt++) {
      await new Promise((resolve) => window.setTimeout(resolve, GIT_ACCOUNT_VERIFY_POLL_INTERVAL_MS))
      const response = await loadGitAccounts({ silent: true })
      const account = response?.accounts?.find((item) => item.id === id)
      if (!account || account.status !== 'verifying') return account
    }
    return null
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

function upsertGitAccount(account) {
  if (!account?.id) return
  const index = gitAccountsState.accounts.findIndex((item) => item.id === account.id)
  if (index >= 0) {
    gitAccountsState.accounts.splice(index, 1, { ...gitAccountsState.accounts[index], ...account })
  } else {
    gitAccountsState.accounts = [account, ...gitAccountsState.accounts]
  }
  if (account.is_default) {
    gitAccountsState.default_account_id = account.id
  } else if (!gitAccountsState.default_account_id && gitAccountsState.accounts.length === 1) {
    gitAccountsState.default_account_id = account.id
  }
  gitAccountsState.loaded = true
}
