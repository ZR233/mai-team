import { reactive } from 'vue'
import { useApi } from './useApi'

const agentConfigState = reactive({
  research_agent: null,
  effective_research_agent: null,
  validation_error: '',
  loading: false,
  saving: false
})

export function useAgentConfig() {
  const { api } = useApi()

  async function loadAgentConfig() {
    agentConfigState.loading = true
    try {
      const response = await api('/agent-config')
      applyAgentConfigResponse(response)
    } finally {
      agentConfigState.loading = false
    }
  }

  async function saveAgentConfig(researchAgent) {
    agentConfigState.saving = true
    try {
      const response = await api('/agent-config', {
        method: 'PUT',
        body: JSON.stringify({ research_agent: researchAgent || null })
      })
      applyAgentConfigResponse(response)
      return response
    } finally {
      agentConfigState.saving = false
    }
  }

  return {
    agentConfigState,
    loadAgentConfig,
    saveAgentConfig
  }
}

function applyAgentConfigResponse(response) {
  agentConfigState.research_agent = response?.research_agent || null
  agentConfigState.effective_research_agent = response?.effective_research_agent || null
  agentConfigState.validation_error = response?.validation_error || ''
}
