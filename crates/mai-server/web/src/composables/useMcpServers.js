import { reactive } from 'vue'
import { useApi } from './useApi'

const mcpServersState = reactive({
  servers: {},
  loading: false,
  saving: false,
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

  async function saveMcpServers(servers) {
    mcpServersState.saving = true
    try {
      const response = await api('/mcp-servers', {
        method: 'PUT',
        body: JSON.stringify({ servers })
      })
      applyMcpServersResponse(response)
      return response
    } finally {
      mcpServersState.saving = false
    }
  }

  return {
    mcpServersState,
    loadMcpServers,
    saveMcpServers
  }
}

function applyMcpServersResponse(response) {
  mcpServersState.servers = response?.servers || {}
  mcpServersState.loaded = true
}
