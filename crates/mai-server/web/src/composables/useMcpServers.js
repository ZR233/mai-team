import { reactive } from 'vue'
import { useApi } from './useApi'

const mcpServersState = reactive({
  servers: [],
  loading: false,
  saving: false,
  rechecking: false,
  loaded: false
})

export function useMcpServers() {
  const { api } = useApi()

  async function loadMcpServers() {
    mcpServersState.loading = true
    try {
      const response = await api('/mcp-servers')
      applyMcpServersResponse(response)
      return response
    } finally {
      mcpServersState.loading = false
    }
  }

  async function saveMcpServers({ userServers, builtinServers, clearSecrets }) {
    mcpServersState.saving = true
    try {
      const response = await api('/mcp-servers', {
        method: 'PUT',
        body: JSON.stringify({ servers: userServers, clear_secrets: clearSecrets || {} })
      })
      applyMcpServersResponse(response)
      if (builtinServers) {
        const builtins = await api('/mcp-servers/builtins', {
          method: 'PUT',
          body: JSON.stringify({ servers: builtinServers })
        })
        applyMcpServersResponse(builtins)
      }
      return response
    } finally {
      mcpServersState.saving = false
    }
  }

  async function recheckMcpServers() {
    mcpServersState.rechecking = true
    try {
      const response = await api('/mcp-servers/recheck', { method: 'POST' })
      applyMcpServersResponse(response)
      return response
    } finally {
      mcpServersState.rechecking = false
    }
  }

  return {
    mcpServersState,
    loadMcpServers,
    saveMcpServers,
    recheckMcpServers
  }
}

function applyMcpServersResponse(response) {
  mcpServersState.servers = response?.servers || []
  mcpServersState.loaded = true
}
