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
const isProjectStopping = ref(false)
const isProjectReviewRunLoading = ref(false)
const projectConversationRef = ref(null)
const projectSkillsState = reactive({
  projectId: null,
  roots: [],
  skills: [],
  errors: [],
  loading: false,
  refreshing: false,
  loaded: false,
  error: ''
})

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
      resetProjectSkillsState()
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
      selectedProjectSessionId.value = selectedProjectDetail.value?.selected_agent?.selected_session_id
        || selectedProjectSessionId.value
        || null
      if (selectedProjectDetail.value?.status === 'ready' || selectedProjectDetail.value?.clone_status === 'ready') {
        ensureProjectSkillsLoaded(selectedProjectId.value).catch(() => {})
      }
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
    resetProjectSkillsState(id)
    await refreshProjects()
    await refreshProjectDetail()
  }

  async function selectProjectAgent(id) {
    if (!selectedProjectId.value || !id) return
    selectedProjectAgentId.value = id
    selectedProjectSessionId.value = null
    await refreshProjectDetail()
  }

  async function selectProjectSession(input) {
    const agentId = typeof input === 'object' ? input?.agentId : null
    const sessionId = typeof input === 'object' ? input?.sessionId : input
    if (!selectedProjectId.value || !sessionId) return
    if (agentId) selectedProjectAgentId.value = agentId
    selectedProjectSessionId.value = sessionId
    await refreshProjectDetail()
  }

  function upsertProjectSummary(project) {
    if (!project?.id) return
    const index = projects.value.findIndex((item) => item.id === project.id)
    if (index >= 0) {
      projects.value.splice(index, 1, { ...projects.value[index], ...project })
    } else {
      projects.value = [project, ...projects.value]
    }
  }

  function refreshProjectCreationState() {
    refreshProjects().catch(() => {})
    if (selectedProjectId.value) {
      refreshProjectDetail().catch(() => {})
    }
  }

  async function createProject(payload) {
    const response = await api('/projects', {
      method: 'POST',
      body: JSON.stringify(payload)
    })
    const project = response?.project || response
    upsertProjectSummary(project)
    selectedProjectId.value = project?.id || null
    selectedProjectAgentId.value = project?.maintainer_agent_id || null
    selectedProjectSessionId.value = null
    selectedProjectDetail.value = project || null
    resetProjectSkillsState(project?.id || null)
    refreshProjectCreationState()
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
      resetProjectSkillsState()
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
          session_id: selectedProjectDetail.value?.maintainer_agent?.selected_session_id || null
        })
      })
      await refreshProjects()
      await refreshProjectDetail()
      return response
    } finally {
      isProjectSending.value = false
    }
  }

  async function loadProjectSkills(projectId = selectedProjectId.value) {
    if (!projectId) return null
    if (projectSkillsState.projectId !== projectId) resetProjectSkillsState(projectId)
    projectSkillsState.loading = true
    projectSkillsState.error = ''
    try {
      const response = await api(`/projects/${projectId}/skills`)
      applyProjectSkillsResponse(projectId, response)
      return response
    } catch (error) {
      projectSkillsState.error = error.message
      throw error
    } finally {
      projectSkillsState.loading = false
    }
  }

  async function detectProjectSkills(projectId = selectedProjectId.value) {
    if (!projectId) return null
    if (projectSkillsState.projectId !== projectId) resetProjectSkillsState(projectId)
    projectSkillsState.refreshing = true
    projectSkillsState.error = ''
    try {
      const response = await api(`/projects/${projectId}/skills/detect`, { method: 'POST' })
      applyProjectSkillsResponse(projectId, response)
      return response
    } catch (error) {
      projectSkillsState.error = error.message
      throw error
    } finally {
      projectSkillsState.refreshing = false
    }
  }

  async function ensureProjectSkillsLoaded(projectId = selectedProjectId.value) {
    if (!projectId || projectSkillsState.loading || projectSkillsState.refreshing) return null
    if (projectSkillsState.projectId === projectId && projectSkillsState.loaded) return projectSkillsState
    return loadProjectSkills(projectId)
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

  async function stopProjectAgentTurn(agent = selectedProjectDetail.value?.selected_agent) {
    const agentId = agent?.id || selectedProjectAgentId.value
    const turnId = agent?.current_turn || selectedProjectDetail.value?.selected_agent?.current_turn
    if (!agentId || !turnId) return null
    isProjectStopping.value = true
    try {
      await api(`/agents/${agentId}/turns/${turnId}/cancel`, { method: 'POST' })
      await refreshProjects()
      await refreshProjectDetail()
    } finally {
      isProjectStopping.value = false
    }
  }

  async function createProjectSession(agent = selectedProjectDetail.value?.selected_agent) {
    const agentId = agent?.id || selectedProjectAgentId.value
    if (!agentId) return null
    selectedProjectAgentId.value = agentId
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

  async function loadProjectReviewRun(runId) {
    if (!selectedProjectId.value || !runId) return null
    isProjectReviewRunLoading.value = true
    try {
      const run = await api(`/projects/${selectedProjectId.value}/review-runs/${runId}`)
      if (selectedProjectDetail.value?.id === selectedProjectId.value) {
        const runs = selectedProjectDetail.value.review_runs || []
        const index = runs.findIndex((item) => item.id === runId)
        if (index >= 0) {
          runs.splice(index, 1, { ...runs[index], ...run, detail_loaded: true })
        }
      }
      return run
    } finally {
      isProjectReviewRunLoading.value = false
    }
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
      branch: ''
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
    isProjectStopping,
    isProjectReviewRunLoading,
    projectConversationRef,
    projectSkillsState,
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
    loadProjectSkills,
    detectProjectSkills,
    ensureProjectSkillsLoaded,
    cancelProject,
    cancelProjectAgent,
    stopProjectAgentTurn,
    createProjectSession,
    updateProjectAgent,
    loadProjectReviewRun,
    loadGitAccountRepositories,
    loadRuntimeDefaults,
    loadGitAccountRepositoryPackages,
    scrollProjectConversationToBottom
  }
}

function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()))
}

function resetProjectSkillsState(projectId = null) {
  projectSkillsState.projectId = projectId
  projectSkillsState.roots = []
  projectSkillsState.skills = []
  projectSkillsState.errors = []
  projectSkillsState.loading = false
  projectSkillsState.refreshing = false
  projectSkillsState.loaded = false
  projectSkillsState.error = ''
}

function applyProjectSkillsResponse(projectId, response) {
  projectSkillsState.projectId = projectId
  projectSkillsState.roots = response?.roots || []
  projectSkillsState.skills = response?.skills || []
  projectSkillsState.errors = response?.errors || []
  projectSkillsState.loaded = true
}
