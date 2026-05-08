import { nextTick, reactive, ref } from 'vue'
import { useApi } from './useApi'
import { highlightCodeBlocks } from '../utils/markdown'

const tasks = ref([])
const selectedTaskId = ref(null)
const selectedAgentId = ref(null)
const selectedTaskDetail = ref(null)
const isLoading = ref(false)
const isSending = ref(false)
const isDetailLoading = ref(false)
const isApprovingPlan = ref(false)
const conversationRef = ref(null)

export function useTasks() {
  const { api, showToast } = useApi()

  async function refreshTasks() {
    const response = await api('/tasks')
    tasks.value = response || []
    if (selectedTaskId.value && !tasks.value.some((task) => task.id === selectedTaskId.value)) {
      selectedTaskId.value = null
      selectedAgentId.value = null
      selectedTaskDetail.value = null
    }
  }

  async function ensureDefaultTask() {
    const response = await api('/tasks:ensure-default', { method: 'POST' })
    if (response?.id && !selectedTaskId.value) {
      selectedTaskId.value = response.id
    }
    await refreshTasks()
    if (selectedTaskId.value) await refreshDetail()
    return response
  }

  async function refreshDetail() {
    if (!selectedTaskId.value) return
    const isFirstLoad = !selectedTaskDetail.value
    if (isFirstLoad) isDetailLoading.value = true
    try {
      const query = selectedAgentId.value ? `?agent_id=${encodeURIComponent(selectedAgentId.value)}` : ''
      selectedTaskDetail.value = await api(`/tasks/${selectedTaskId.value}${query}`)
      selectedAgentId.value = selectedTaskDetail.value?.selected_agent_id || null
      await nextTick()
      highlightCodeBlocks(conversationRef.value)
      await scrollConversationToBottom()
    } finally {
      if (isFirstLoad) isDetailLoading.value = false
    }
  }

  async function selectTask(id) {
    selectedTaskId.value = id
    selectedAgentId.value = null
    await refreshTasks()
    await refreshDetail()
  }

  async function selectTaskAgent(id) {
    if (!selectedTaskId.value || !id) return
    selectedAgentId.value = id
    await refreshDetail()
  }

  async function createTask(message, dockerImage) {
    const response = await api('/tasks', {
      method: 'POST',
      body: JSON.stringify({
        message: message || null,
        docker_image: dockerImage || null
      })
    })
    selectedTaskId.value = response.task.id
    selectedAgentId.value = response.task.planner_agent_id
    refreshTasks()
    refreshDetail()
    return response.task
  }

  async function sendTaskMessage(message, skillMentions = []) {
    if (!selectedTaskId.value || !message) return null
    isSending.value = true
    try {
      const response = await api(`/tasks/${selectedTaskId.value}/messages`, {
        method: 'POST',
        body: JSON.stringify({ message, skill_mentions: skillMentions })
      })
      await refreshTasks()
      await refreshDetail()
      return response
    } finally {
      isSending.value = false
    }
  }

  async function approveTaskPlan() {
    if (!selectedTaskId.value) return null
    isApprovingPlan.value = true
    try {
      const response = await api(`/tasks/${selectedTaskId.value}/plan:approve`, { method: 'POST' })
      await refreshTasks()
      await refreshDetail()
      return response.task
    } finally {
      isApprovingPlan.value = false
    }
  }

  async function requestPlanRevision(feedback) {
    if (!selectedTaskId.value || !feedback) return null
    isSending.value = true
    try {
      const response = await api(`/tasks/${selectedTaskId.value}/plan:request-revision`, {
        method: 'POST',
        body: JSON.stringify({ feedback })
      })
      await refreshTasks()
      await refreshDetail()
      return response.task
    } finally {
      isSending.value = false
    }
  }

  async function cancelTask(id) {
    await api(`/tasks/${id}/cancel`, { method: 'POST' })
    await refreshTasks()
    if (selectedTaskId.value === id) await refreshDetail()
  }

  async function cancelTaskAgent(id) {
    await api(`/agents/${id}/cancel`, { method: 'POST' })
    await refreshDetail()
  }

  async function deleteTask(id) {
    await api(`/tasks/${id}`, { method: 'DELETE' })
    if (selectedTaskId.value === id) {
      selectedTaskId.value = null
      selectedAgentId.value = null
      selectedTaskDetail.value = null
    }
    await refreshTasks()
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
    await refreshTasks()
    await refreshDetail()
    return response.agent
  }

  async function scrollConversationToBottom() {
    await nextTick()
    await nextFrame()
    if (conversationRef.value) {
      conversationRef.value.scrollTop = conversationRef.value.scrollHeight
    }
  }

  const taskDialog = reactive({
    open: false,
    message: '',
    docker_image: '',
    error: '',
    submitting: false
  })

  return {
    tasks,
    selectedTaskId,
    selectedAgentId,
    selectedTaskDetail,
    isLoading,
    isSending,
    isDetailLoading,
    isApprovingPlan,
    conversationRef,
    taskDialog,
    refreshTasks,
    ensureDefaultTask,
    refreshDetail,
    selectTask,
    selectTaskAgent,
    createTask,
    sendTaskMessage,
    approveTaskPlan,
    requestPlanRevision,
    cancelTask,
    cancelTaskAgent,
    deleteTask,
    updateAgent,
    scrollConversationToBottom,
    showToast
  }
}

function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()))
}
