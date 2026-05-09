import { ref } from 'vue'

const toast = ref('')

function readError(text) {
  try {
    return JSON.parse(text).error || text
  } catch {
    return text
  }
}

export function useApi() {
  async function api(path, init = {}) {
    const { timeoutMs, ...requestInit } = init
    const headers = {
      ...(requestInit.body ? { 'content-type': 'application/json' } : {}),
      ...(requestInit.headers || {})
    }
    const response = await fetchWithTimeout(path, { ...requestInit, headers }, timeoutMs)
    if (!response.ok) {
      const text = await response.text()
      throw new Error(readError(text))
    }
    if (response.status === 204) return null
    return response.json()
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
    showToast
  }
}

async function fetchWithTimeout(path, init, timeoutMs) {
  if (!timeoutMs) return fetch(path, init)
  const controller = new AbortController()
  const timer = window.setTimeout(() => controller.abort(), timeoutMs)
  try {
    return await fetch(path, { ...init, signal: controller.signal })
  } catch (error) {
    if (error?.name === 'AbortError') {
      throw new Error('Request timed out.')
    }
    throw error
  } finally {
    window.clearTimeout(timer)
  }
}
