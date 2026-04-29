import { nextTick, reactive, ref } from 'vue'
import { useApi } from './useApi'
import { highlightCodeBlocks } from '../utils/markdown'

const agents = ref([])
const selectedAgentId = ref(null)
const selectedDetail = ref(null)
const isLoading = ref(false)
const isSending = ref(false)
const isDetailLoading = ref(false)
const conversationRef = ref(null)

export function useAgents() {
  const { api, showToast } = useApi()

  async function refreshAgents() {
    const response = await api('/agents')
    agents.value = response
    if (selectedAgentId.value && !agents.value.some((agent) => agent.id === selectedAgentId.value)) {
      selectedAgentId.value = null
      selectedDetail.value = null
    }
  }

  async function refreshDetail() {
    if (!selectedAgentId.value) return
    isDetailLoading.value = true
    try {
      selectedDetail.value = await api(`/agents/${selectedAgentId.value}`)
      await nextTick()
      highlightCodeBlocks(conversationRef.value)
      await scrollConversationToBottom()
    } finally {
      isDetailLoading.value = false
    }
  }

  async function scrollConversationToBottom() {
    await nextTick()
    await nextFrame()
    if (conversationRef.value) {
      conversationRef.value.scrollTop = conversationRef.value.scrollHeight
    }
  }

  const scrollToBottom = scrollConversationToBottom

  async function selectAgent(id) {
    selectedAgentId.value = id
    await refreshAgents()
    await refreshDetail()
  }

  async function createAgent(name, providerId, model) {
    const response = await api('/agents', {
      method: 'POST',
      body: JSON.stringify({
        name: name || null,
        provider_id: providerId || null,
        model: model || null
      })
    })
    selectedAgentId.value = response.agent.id
    await refreshAgents()
    await refreshDetail()
    return response.agent
  }

  async function sendMessage(message) {
    if (!selectedAgentId.value || !message) return
    isSending.value = true
    try {
      await api(`/agents/${selectedAgentId.value}/messages`, {
        method: 'POST',
        body: JSON.stringify({ message })
      })
      await refreshDetail()
    } finally {
      isSending.value = false
    }
  }

  async function cancelAgent(id) {
    await api(`/agents/${id}/cancel`, { method: 'POST' })
    await refreshDetail()
  }

  async function deleteAgent(id) {
    await api(`/agents/${id}`, { method: 'DELETE' })
    if (selectedAgentId.value === id) {
      selectedAgentId.value = null
      selectedDetail.value = null
    }
    await refreshAgents()
  }

  const agentDialog = reactive({
    open: false,
    name: '',
    provider_id: '',
    model: '',
    error: ''
  })

  return {
    agents,
    selectedAgentId,
    selectedDetail,
    isLoading,
    isSending,
    isDetailLoading,
    conversationRef,
    agentDialog,
    refreshAgents,
    refreshDetail,
    selectAgent,
    createAgent,
    sendMessage,
    cancelAgent,
    deleteAgent,
    scrollToBottom,
    scrollConversationToBottom,
    showToast
  }
}

function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()))
}
