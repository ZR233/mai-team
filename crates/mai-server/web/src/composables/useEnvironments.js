import { nextTick, reactive, ref } from 'vue'
import { useApi } from './useApi.js'
import { highlightCodeBlocks } from '../utils/markdown.js'

const environments = ref([])
const selectedEnvironmentId = ref(null)
const selectedConversationId = ref(null)
const selectedEnvironmentDetail = ref(null)
const isEnvironmentsLoading = ref(false)
const isEnvironmentDetailLoading = ref(false)
const isEnvironmentSending = ref(false)
const isEnvironmentStopping = ref(false)
const environmentConversationRef = ref(null)
let environmentsRequestSeq = 0
let environmentDetailRequestSeq = 0

export function useEnvironments() {
  const { api } = useApi()

  async function refreshEnvironments() {
    const requestSeq = ++environmentsRequestSeq
    const response = await api('/environments')
    if (requestSeq !== environmentsRequestSeq) return
    environments.value = response || []
    if (selectedEnvironmentId.value && !environments.value.some((environment) => environment.id === selectedEnvironmentId.value)) {
      selectedEnvironmentId.value = null
      selectedConversationId.value = null
      selectedEnvironmentDetail.value = null
    }
  }

  async function ensureDefaultEnvironment() {
    const response = await api('/environments:ensure-default', { method: 'POST' })
    if (response?.id && !selectedEnvironmentId.value) {
      selectedEnvironmentId.value = response.id
    }
    await refreshEnvironments()
    if (selectedEnvironmentId.value) await refreshEnvironmentDetail()
    return response
  }

  async function refreshEnvironmentDetail() {
    if (!selectedEnvironmentId.value) return
    const requestSeq = ++environmentDetailRequestSeq
    const environmentId = selectedEnvironmentId.value
    const conversationId = selectedConversationId.value
    const isFirstLoad = !selectedEnvironmentDetail.value
    if (isFirstLoad) isEnvironmentDetailLoading.value = true
    try {
      const query = conversationId ? `?session_id=${encodeURIComponent(conversationId)}` : ''
      const detail = await api(`/environments/${environmentId}${query}`)
      if (
        requestSeq !== environmentDetailRequestSeq ||
        selectedEnvironmentId.value !== environmentId ||
        selectedConversationId.value !== conversationId
      ) {
        return
      }
      selectedEnvironmentDetail.value = normalizeEnvironmentDetail(detail)
      selectedConversationId.value = selectedEnvironmentDetail.value?.selected_conversation_id
        || selectedEnvironmentDetail.value?.root_agent?.selected_session_id
        || selectedConversationId.value
        || null
      await nextTick()
      highlightCodeBlocks(environmentConversationRef.value)
      await scrollEnvironmentConversationToBottom()
    } finally {
      if (isFirstLoad) isEnvironmentDetailLoading.value = false
    }
  }

  async function selectEnvironment(id) {
    if (!id) return
    selectedEnvironmentId.value = id
    selectedConversationId.value = null
    selectedEnvironmentDetail.value = null
    await refreshEnvironments()
    await refreshEnvironmentDetail()
  }

  async function selectConversation(id) {
    if (!selectedEnvironmentId.value || !id) return
    selectedConversationId.value = id
    await refreshEnvironmentDetail()
  }

  function upsertEnvironmentSummary(environment) {
    if (!environment?.id) return
    const index = environments.value.findIndex((item) => item.id === environment.id)
    if (index >= 0) {
      environments.value.splice(index, 1, { ...environments.value[index], ...environment })
    } else {
      environments.value = [environment, ...environments.value]
    }
  }

  async function createEnvironment(name, dockerImage) {
    const response = await api('/environments', {
      method: 'POST',
      body: JSON.stringify({
        name: name || null,
        docker_image: dockerImage || null
      })
    })
    const environment = response?.environment || response
    upsertEnvironmentSummary(environment)
    selectedEnvironmentId.value = environment?.id || null
    selectedConversationId.value = null
    selectedEnvironmentDetail.value = null
    await refreshEnvironments()
    await refreshEnvironmentDetail()
    return environment
  }

  async function createConversation() {
    if (!selectedEnvironmentId.value) return null
    const response = await api(`/environments/${selectedEnvironmentId.value}/conversations`, { method: 'POST' })
    const conversation = response?.conversation || response?.session || response
    selectedConversationId.value = conversation?.id || null
    await refreshEnvironments()
    await refreshEnvironmentDetail()
    return conversation
  }

  async function sendEnvironmentMessage(message, skillMentions = []) {
    if (!selectedEnvironmentId.value || !selectedConversationId.value || !message) return null
    isEnvironmentSending.value = true
    try {
      const response = await api(
        `/environments/${selectedEnvironmentId.value}/conversations/${selectedConversationId.value}/messages`,
        {
          method: 'POST',
          body: JSON.stringify({
            message,
            skill_mentions: skillMentions
          })
        }
      )
      await refreshEnvironments()
      await refreshEnvironmentDetail()
      return response
    } finally {
      isEnvironmentSending.value = false
    }
  }

  async function stopEnvironmentAgentTurn(agent = selectedEnvironmentDetail.value?.root_agent) {
    const agentId = agent?.id || selectedEnvironmentDetail.value?.root_agent?.id
    const turnId = agent?.current_turn || selectedEnvironmentDetail.value?.root_agent?.current_turn
    if (!agentId || !turnId) return null
    isEnvironmentStopping.value = true
    try {
      await api(`/agents/${agentId}/turns/${turnId}/cancel`, { method: 'POST' })
      await refreshEnvironments()
      await refreshEnvironmentDetail()
    } finally {
      isEnvironmentStopping.value = false
    }
  }

  async function updateAgent(id, providerId, model, reasoningEffort) {
    const response = await api(`/agents/${id}`, {
      method: 'PATCH',
      body: JSON.stringify({
        provider_id: providerId || null,
        model: model || null,
        reasoning_effort: reasoningEffort || null
      })
    })
    await refreshEnvironmentDetail()
    return response?.agent || response
  }

  async function scrollEnvironmentConversationToBottom() {
    await nextTick()
    await nextFrame()
    if (environmentConversationRef.value) {
      environmentConversationRef.value.scrollTop = environmentConversationRef.value.scrollHeight
    }
  }

  const environmentDialog = reactive({
    open: false,
    name: '',
    docker_image: '',
    error: '',
    submitting: false
  })

  return {
    environments,
    selectedEnvironmentId,
    selectedConversationId,
    selectedEnvironmentDetail,
    isEnvironmentsLoading,
    isEnvironmentDetailLoading,
    isEnvironmentSending,
    isEnvironmentStopping,
    environmentConversationRef,
    environmentDialog,
    refreshEnvironments,
    ensureDefaultEnvironment,
    refreshEnvironmentDetail,
    selectEnvironment,
    selectConversation,
    createEnvironment,
    createConversation,
    sendEnvironmentMessage,
    stopEnvironmentAgentTurn,
    updateAgent,
    scrollEnvironmentConversationToBottom
  }
}

function normalizeEnvironmentDetail(detail) {
  if (!detail) return null
  const selectedConversationId = detail.selected_conversation_id
    || detail.root_agent?.selected_session_id
    || detail.conversations?.[0]?.id
    || null
  const conversations = detail.conversations || detail.root_agent?.sessions || []
  const rootAgent = detail.root_agent
    ? {
        ...detail.root_agent,
        selected_session_id: selectedConversationId,
        sessions: detail.root_agent.sessions || conversations
      }
    : null
  return {
    ...detail,
    selected_conversation_id: selectedConversationId,
    conversations,
    root_agent: rootAgent
  }
}

function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()))
}
