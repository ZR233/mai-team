import { nextTick, reactive, ref } from 'vue'
import { useApi } from './useApi'
import { highlightCodeBlocks } from '../utils/markdown'

const projects = ref([])
const selectedProjectId = ref(null)
const selectedProjectAgentId = ref(null)
const selectedProjectSessionId = ref(null)
const selectedProjectDetail = ref(null)
const isProjectsLoading = ref(false)
const isProjectDetailLoading = ref(false)
const isProjectSending = ref(false)
const projectConversationRef = ref(null)

export function useProjects() {
  const { api } = useApi()

  async function refreshProjects() {
    const response = await api('/projects')
    projects.value = response || []
    if (selectedProjectId.value && !projects.value.some((project) => project.id === selectedProjectId.value)) {
      selectedProjectId.value = null
      selectedProjectAgentId.value = null
      selectedProjectSessionId.value = null
      selectedProjectDetail.value = null
    }
  }

  async function refreshProjectDetail() {
    if (!selectedProjectId.value) return
    const isFirstLoad = !selectedProjectDetail.value
    if (isFirstLoad) isProjectDetailLoading.value = true
    try {
      const params = new URLSearchParams()
      if (selectedProjectAgentId.value) params.set('agent_id', selectedProjectAgentId.value)
      if (selectedProjectSessionId.value) params.set('session_id', selectedProjectSessionId.value)
      const query = params.toString() ? `?${params.toString()}` : ''
      selectedProjectDetail.value = await api(`/projects/${selectedProjectId.value}${query}`)
      selectedProjectAgentId.value = selectedProjectDetail.value?.selected_agent_id
        || selectedProjectDetail.value?.maintainer_agent?.id
        || selectedProjectAgentId.value
        || null
      selectedProjectSessionId.value = selectedProjectDetail.value?.maintainer_agent?.selected_session_id
        || selectedProjectSessionId.value
        || null
      await nextTick()
      highlightCodeBlocks(projectConversationRef.value)
      await scrollProjectConversationToBottom()
    } finally {
      if (isFirstLoad) isProjectDetailLoading.value = false
    }
  }

  async function selectProject(id) {
    selectedProjectId.value = id
    selectedProjectAgentId.value = null
    selectedProjectSessionId.value = null
    selectedProjectDetail.value = null
    await refreshProjects()
    await refreshProjectDetail()
  }

  async function selectProjectAgent(id) {
    if (!selectedProjectId.value || !id) return
    selectedProjectAgentId.value = id
    selectedProjectSessionId.value = null
    await refreshProjectDetail()
  }

  async function selectProjectSession(id) {
    if (!selectedProjectId.value || !id) return
    selectedProjectSessionId.value = id
    await refreshProjectDetail()
  }

  async function createProject(payload) {
    const response = await api('/projects', {
      method: 'POST',
      body: JSON.stringify(payload)
    })
    const project = response?.project || response
    selectedProjectId.value = project?.id || null
    selectedProjectAgentId.value = project?.maintainer_agent_id || null
    selectedProjectSessionId.value = null
    await refreshProjects()
    if (selectedProjectId.value) await refreshProjectDetail()
    return project
  }

  async function updateProject(id, patch) {
    const response = await api(`/projects/${id}`, {
      method: 'PATCH',
      body: JSON.stringify(patch)
    })
    await refreshProjects()
    if (selectedProjectId.value === id) await refreshProjectDetail()
    return response?.project || response
  }

  async function deleteProject(id) {
    await api(`/projects/${id}`, { method: 'DELETE' })
    if (selectedProjectId.value === id) {
      selectedProjectId.value = null
      selectedProjectAgentId.value = null
      selectedProjectSessionId.value = null
      selectedProjectDetail.value = null
    }
    await refreshProjects()
  }

  async function sendProjectMessage(message, skillMentions = []) {
    if (!selectedProjectId.value || !message) return null
    isProjectSending.value = true
    try {
      const response = await api(`/projects/${selectedProjectId.value}/messages`, {
        method: 'POST',
        body: JSON.stringify({
          message,
          skill_mentions: skillMentions,
          session_id: selectedProjectSessionId.value || selectedProjectDetail.value?.maintainer_agent?.selected_session_id || null
        })
      })
      await refreshProjects()
      await refreshProjectDetail()
      return response
    } finally {
      isProjectSending.value = false
    }
  }

  async function cancelProject(id) {
    await api(`/projects/${id}/cancel`, { method: 'POST' })
    await refreshProjects()
    if (selectedProjectId.value === id) await refreshProjectDetail()
  }

  async function cancelProjectAgent(id) {
    await api(`/agents/${id}/cancel`, { method: 'POST' })
    await refreshProjectDetail()
  }

  async function createProjectSession() {
    const agentId = selectedProjectDetail.value?.maintainer_agent?.id || selectedProjectAgentId.value
    if (!agentId) return null
    const response = await api(`/agents/${agentId}/sessions`, { method: 'POST' })
    selectedProjectSessionId.value = response?.session?.id || response?.id || null
    await refreshProjectDetail()
    return response
  }

  async function updateProjectAgent(id, providerId, model, reasoningEffort) {
    const response = await api(`/agents/${id}`, {
      method: 'PATCH',
      body: JSON.stringify({
        provider_id: providerId || null,
        model: model || null,
        reasoning_effort: reasoningEffort || null
      })
    })
    await refreshProjectDetail()
    return response?.agent || response
  }

  async function loadGitAccountRepositories(accountId) {
    if (!accountId) return { repositories: [] }
    return api(`/git/accounts/${encodeURIComponent(accountId)}/repositories`)
  }

  async function loadRuntimeDefaults() {
    return api('/runtime/defaults')
  }

  async function loadGitAccountRepositoryPackages(accountId, repositoryFullName) {
    if (!accountId || !repositoryFullName || !repositoryFullName.includes('/')) {
      return { packages: [], warning: null }
    }
    const [owner, ...repoParts] = repositoryFullName.split('/')
    const repo = repoParts.join('/')
    if (!owner || !repo) return { packages: [], warning: null }
    return api(`/git/accounts/${encodeURIComponent(accountId)}/repositories/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}/packages`)
  }

  async function scrollProjectConversationToBottom() {
    await nextTick()
    await nextFrame()
    if (projectConversationRef.value) {
      projectConversationRef.value.scrollTop = projectConversationRef.value.scrollHeight
    }
  }

  const projectDialog = reactive({
    open: false,
    mode: 'git_account',
    form: {
      name: '',
      git_account_id: '',
      repository_full_name: '',
      branch: '',
      project_path: '/'
    },
    repository: {
      query: ''
    },
    runtime: {
      docker_image: '',
      default_docker_image: '',
      packages: [],
      package_image: '',
      loadingPackages: false,
      packageWarning: ''
    },
    gitAccounts: [],
    repositories: [],
    loadingAccounts: false,
    loadingRepositories: false,
    submitting: false,
    error: ''
  })

  return {
    projects,
    selectedProjectId,
    selectedProjectAgentId,
    selectedProjectSessionId,
    selectedProjectDetail,
    isProjectsLoading,
    isProjectDetailLoading,
    isProjectSending,
    projectConversationRef,
    projectDialog,
    refreshProjects,
    refreshProjectDetail,
    selectProject,
    selectProjectAgent,
    selectProjectSession,
    createProject,
    updateProject,
    deleteProject,
    sendProjectMessage,
    cancelProject,
    cancelProjectAgent,
    createProjectSession,
    updateProjectAgent,
    loadGitAccountRepositories,
    loadRuntimeDefaults,
    loadGitAccountRepositoryPackages,
    scrollProjectConversationToBottom
  }
}

function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()))
}
