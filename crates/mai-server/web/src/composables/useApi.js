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
    const headers = {
      ...(init.body ? { 'content-type': 'application/json' } : {}),
      ...(init.headers || {})
    }
    const response = await fetch(path, { ...init, headers })
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
