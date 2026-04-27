import { ref } from 'vue'

const toast = ref('')
let token = localStorage.getItem('maiToken') || new URLSearchParams(window.location.search).get('token') || ''
let tokenResolver = null

function readError(text) {
  try {
    return JSON.parse(text).error || text
  } catch {
    return text
  }
}

export function useApi() {
  async function api(path, init = {}, retry = true) {
    if (!token) {
      await openTokenDialog('Bearer Token')
    }
    const headers = {
      authorization: `Bearer ${token}`,
      ...(init.body ? { 'content-type': 'application/json' } : {}),
      ...(init.headers || {})
    }
    const response = await fetch(path, { ...init, headers })
    if (response.status === 401 && retry) {
      await openTokenDialog('Token expired or invalid')
      return api(path, init, false)
    }
    if (!response.ok) {
      const text = await response.text()
      throw new Error(readError(text))
    }
    if (response.status === 204) return null
    return response.json()
  }

  function getToken() {
    return token
  }

  function setToken(newToken) {
    token = newToken
    localStorage.setItem('maiToken', newToken)
  }

  function openTokenDialog(title) {
    const result = { resolve: null }
    const promise = new Promise((resolve) => {
      result.resolve = resolve
    })
    tokenResolver = result.resolve
    return { promise, title, resolve: result.resolve }
  }

  function saveToken(newToken, resolveCallback) {
    token = newToken.trim()
    localStorage.setItem('maiToken', token)
    if (resolveCallback) resolveCallback()
  }

  function showToast(message) {
    toast.value = message
    window.clearTimeout(showToast.timer)
    showToast.timer = window.setTimeout(() => {
      toast.value = ''
    }, 4000)
  }

  return {
    api,
    toast,
    getToken,
    setToken,
    openTokenDialog,
    saveToken,
    showToast
  }
}
