import { reactive } from 'vue'
import { useApi } from './useApi'

const agentConfigState = reactive({
  planner: null,
  explorer: null,
  executor: null,
  reviewer: null,
  effective_planner: null,
  effective_explorer: null,
  effective_executor: null,
  effective_reviewer: null,
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

  async function saveAgentConfig(config) {
    agentConfigState.saving = true
    try {
      const response = await api('/agent-config', {
        method: 'PUT',
        body: JSON.stringify({
          planner: config?.planner || null,
          explorer: config?.explorer || null,
          executor: config?.executor || null,
          reviewer: config?.reviewer || null
        })
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
  agentConfigState.planner = response?.planner || null
  agentConfigState.explorer = response?.explorer || null
  agentConfigState.executor = response?.executor || response?.research_agent || null
  agentConfigState.reviewer = response?.reviewer || null
  agentConfigState.effective_planner = response?.effective_planner || null
  agentConfigState.effective_explorer = response?.effective_explorer || null
  agentConfigState.effective_executor = response?.effective_executor || response?.effective_research_agent || null
  agentConfigState.effective_reviewer = response?.effective_reviewer || null
  agentConfigState.validation_error = response?.validation_error || ''
}
