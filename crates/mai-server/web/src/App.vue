<template>
  <div class="app-shell">
    <header class="topbar">
      <div class="brand">
        <div class="brand-mark">M</div>
        <div>
          <h1>Mai Team</h1>
          <p>Task-oriented multi-agent console</p>
        </div>
      </div>
      <nav class="tabs" aria-label="Views">
        <button :class="{ active: activeTab === 'tasks' }" @click="activeTab = 'tasks'">Tasks</button>
        <button :class="{ active: activeTab === 'projects' }" @click="activeTab = 'projects'">Projects</button>
        <button :class="{ active: activeTab === 'providers' }" @click="activeTab = 'providers'">Providers</button>
        <button :class="{ active: activeTab === 'settings' }" @click="activeTab = 'settings'">Settings</button>
      </nav>
      <div class="topbar-actions">
        <span class="connection" :class="connectionState">
          <span class="connection-dot" />
          {{ connectionLabel }}
        </span>
        <button class="primary-button" :disabled="isLoading" @click="refreshAll">
          <span v-if="isLoading" class="spinner-sm"></span>
          <template v-else>Refresh</template>
        </button>
      </div>
    </header>

    <main class="workspace">
      <TaskRail
        :tasks="tasks"
        :detail="selectedTaskDetail"
        :selected-task-id="selectedTaskId"
        :selected-agent-id="selectedAgentId"
        :visible="activeTab === 'tasks'"
        @select-task="selectTask"
        @select-agent="selectTaskAgent"
        @create="openCreateTaskDialog"
      />

      <ProjectWorkspace
        v-if="activeTab === 'projects'"
        v-model:conversation-ref="projectConversationRef"
        :projects="projects"
        :detail="selectedProjectDetail"
        :selected-project-id="selectedProjectId"
        :events="eventFeed"
        :draft="projectMessageDraft"
        :loading="isProjectDetailLoading"
        :sending="isProjectSending"
        :providers="providersState.providers"
        :skills="enabledSkills"
        :selected-skills="selectedProjectSkills"
        :skills-loading="skillsState.loading"
        :skills-error="skillsError"
        :updating-model="isUpdatingProjectAgentModel"
        @create="openCreateProjectDialog"
        @select-project="selectProject"
        @select-agent="selectProjectAgent"
        @cancel-agent="onCancelProjectAgent"
        @delete-agent="confirmDeleteProjectAgent"
        @send="onSendProjectMessage"
        @update-model="onUpdateProjectAgentModel"
        @update:draft="projectMessageDraft = $event"
        @update:selected-skills="selectedProjectSkills = $event"
        @load-skills="onLoadSkills"
        @create-session="onCreateProjectSession"
        @select-session="selectProjectSession"
      />

      <section v-else-if="activeTab === 'tasks'" class="agent-stage">
        <div v-if="!selectedTaskDetail" class="empty-stage">
          <div class="empty-mark">+</div>
          <h2>No task selected</h2>
          <p>Create a task or choose one from the left rail.</p>
          <button class="primary-button" @click="openCreateTaskDialog">Create Task</button>
        </div>

        <TaskDetail
          v-else
          :detail="selectedTaskDetail"
          :events="eventFeed"
          :draft="messageDraft"
          :loading="isDetailLoading"
          :sending="isSending"
          :approving-plan="isApprovingPlan"
          :providers="providersState.providers"
          :skills="enabledSkills"
          :selected-skills="selectedSkills"
          :skills-loading="skillsState.loading"
          :skills-error="skillsError"
          :updating-model="isUpdatingAgentModel"
          v-model:conversation-ref="conversationRef"
          @approve-plan="onApprovePlan"
          @request-plan-revision="onRequestPlanRevision"
          @cancel="confirmCancelTask"
          @cancel-agent="onCancelSelectedAgent"
          @delete="confirmDeleteTask"
          @delete-agent="confirmDeleteTaskAgent"
          @send="onSendMessage"
          @update-model="onUpdateAgentModel"
          @update:draft="messageDraft = $event"
          @update:selected-skills="selectedSkills = $event"
          @load-skills="onLoadSkills"
        />
      </section>

      <ProviderGrid
        v-else-if="activeTab === 'providers'"
        :providers="providersState.providers"
        :default-id="providersState.default_provider_id"
        @add="openProviderDialog(null)"
        @edit="openProviderDialog"
        @delete="confirmDeleteProvider"
      />

      <ResearchAgentConfigPanel
        v-else
        :providers="providersState.providers"
        :state="agentConfigState"
        :skills-state="skillsState"
        :loading="agentConfigState.loading"
        :saving="agentConfigState.saving"
        :skills-saving="skillsState.saving"
        :skills-error="skillsError"
        :mcp-servers-state="mcpServersState"
        :mcp-saving="mcpServersState.saving"
        :github-state="githubSettingsState"
        :github-saving="githubSettingsState.saving"
        :github-app-saving="githubSettingsState.appSaving"
        :github-app-manifest-starting="githubSettingsState.manifestStarting"
        :initial-section="settingsInitialSection"
        @reload="loadAgentConfig"
        @save="onSaveAgentConfig"
        @reload-skills="onLoadSkills"
        @save-skills="onSaveSkillsConfig"
        @reload-mcp="onLoadMcpServers"
        @open-mcp="mcpDialogOpen = true"
        @open-providers="activeTab = 'providers'"
        @save-github="onSaveGithubSettings"
        @save-github-app="onSaveGithubAppSettings"
        @configure-github-app="onConfigureGithubApp"
        @refresh-github-installations="onRefreshGithubAppInstallations"
      />
    </main>

    <ProviderDialog
      :dialog="providerDialog"
      @close="closeProviderDialog"
      @save="saveProviderDialog"
      @kind-changed="fillFromPreset"
    />

    <TaskDialog
      :dialog="taskDialog"
      @close="taskDialog.open = false"
      @create="onCreateTask"
    />

    <ProjectDialog
      :dialog="projectDialog"
      :github-state="githubSettingsState"
      @close="projectDialog.open = false"
      @create="onCreateProject"
      @configure-github-app="openGithubAppSettings"
      @install-github-app="openGithubAppInstallation"
      @refresh-installations="onRefreshProjectInstallations"
      @refresh-repositories="onRefreshProjectRepositories"
    />

    <McpServersDialog
      :open="mcpDialogOpen"
      :servers-state="mcpServersState"
      :saving="mcpServersState.saving"
      @close="mcpDialogOpen = false"
      @save="onSaveMcpServers"
    />

    <ConfirmDialog
      :open="confirmDialog.open"
      :title="confirmDialog.title"
      :message="confirmDialog.message"
      @close="confirmDialog.open = false"
      @confirm="onConfirmAction"
    />

    <Transition name="toast">
      <div v-if="toast" class="toast">{{ toast }}</div>
    </Transition>
  </div>
</template>

<script setup>
import { computed, nextTick, onMounted, onUnmounted, reactive, ref, watch } from 'vue'
import { highlightCodeBlocks } from './utils/markdown'

import TaskRail from './components/TaskRail.vue'
import TaskDetail from './components/TaskDetail.vue'
import ProjectWorkspace from './components/ProjectWorkspace.vue'
import ProviderGrid from './components/ProviderGrid.vue'
import ConfirmDialog from './components/ConfirmDialog.vue'
import ProviderDialog from './components/ProviderDialog.vue'
import TaskDialog from './components/TaskDialog.vue'
import ProjectDialog from './components/ProjectDialog.vue'
import ResearchAgentConfigPanel from './components/ResearchAgentConfigPanel.vue'
import McpServersDialog from './components/McpServersDialog.vue'

import { useApi } from './composables/useApi'
import { useSSE } from './composables/useSSE'
import { useTasks } from './composables/useTasks'
import { useProjects } from './composables/useProjects'
import { useProviders } from './composables/useProviders'
import { useAgentConfig } from './composables/useAgentConfig'
import { useSkills } from './composables/useSkills'
import { useMcpServers } from './composables/useMcpServers'
import { useGithubSettings } from './composables/useGithubSettings'

const { toast, showToast } = useApi()
const { eventFeed, connectionState, connectEvents, disconnect } = useSSE()
const {
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
  scrollConversationToBottom
} = useTasks()
const {
  projects,
  selectedProjectId,
  selectedProjectDetail,
  isProjectsLoading,
  isProjectSending,
  isProjectDetailLoading,
  projectConversationRef,
  projectDialog,
  refreshProjects,
  refreshProjectDetail,
  selectProject,
  selectProjectAgent,
  selectProjectSession,
  createProject,
  sendProjectMessage,
  cancelProjectAgent,
  createProjectSession,
  updateProjectAgent,
  loadGithubInstallations,
  refreshGithubInstallations,
  loadGithubRepositories,
  scrollProjectConversationToBottom
} = useProjects()
const {
  providersState,
  providerDialog,
  loadProviders,
  removeProvider,
  openProviderDialog,
  closeProviderDialog,
  saveProviderDialog,
  fillFromPreset
} = useProviders()
const { agentConfigState, loadAgentConfig, saveAgentConfig } = useAgentConfig()
const {
  skillsState,
  enabledSkills,
  loadSkills,
  ensureSkillsLoaded,
  saveSkillsConfig
} = useSkills()
const {
  mcpServersState,
  loadMcpServers,
  saveMcpServers
} = useMcpServers()
const {
  githubSettingsState,
  loadGithubSettings,
  saveGithubSettings,
  saveGithubAppSettings,
  startGithubAppManifest
} = useGithubSettings()

const activeTab = ref('tasks')
const messageDraft = ref('')
const projectMessageDraft = ref('')
const selectedSkills = ref([])
const selectedProjectSkills = ref([])
const isUpdatingAgentModel = ref(false)
const isUpdatingProjectAgentModel = ref(false)
const skillsError = ref('')
const mcpDialogOpen = ref(false)
const settingsInitialSection = ref('roles')

const confirmDialog = reactive({
  open: false,
  title: '',
  message: '',
  onConfirm: () => {}
})

const connectionLabel = computed(() => {
  if (connectionState.value === 'online') return 'Connected'
  if (connectionState.value === 'connecting') return 'Connecting'
  return 'Offline'
})

watch(
  () => [
    selectedTaskDetail.value?.selected_agent?.messages?.length,
    selectedTaskDetail.value?.selected_agent?.recent_events?.length,
    selectedTaskDetail.value?.plan?.version,
    eventFeed.value.length
  ],
  async () => {
    await nextTick()
    highlightCodeBlocks(conversationRef.value)
    await scrollConversationToBottom()
  }
)

watch(
  () => [
    selectedProjectDetail.value?.maintainer_agent?.messages?.length,
    selectedProjectDetail.value?.maintainer_agent?.recent_events?.length,
    selectedProjectDetail.value?.maintainer_agent?.selected_session_id,
    eventFeed.value.length
  ],
  async () => {
    await nextTick()
    highlightCodeBlocks(projectConversationRef.value)
    await scrollProjectConversationToBottom()
  }
)

onMounted(async () => {
  connectEvents(handleSSEEvent)
  applyStartupHash()
  await refreshAll()
})

onUnmounted(() => disconnect())

async function refreshAll() {
  isLoading.value = true
  isProjectsLoading.value = true
  try {
    await Promise.all([loadProviders(), loadAgentConfig(), loadSkills(), loadMcpServers(), loadGithubSettings(), refreshTasks(), refreshProjects()])
    if (providersState.providers.length && !tasks.value.length) {
      await ensureDefaultTask()
    } else if (selectedTaskId.value) {
      await refreshDetail()
    } else if (tasks.value[0]?.id) {
      await selectTask(tasks.value[0].id)
    }
    if (selectedProjectId.value) {
      await refreshProjectDetail()
    } else if (projects.value[0]?.id) {
      await selectProject(projects.value[0].id)
    }
  } catch (error) {
    showToast(error.message)
  } finally {
    isLoading.value = false
    isProjectsLoading.value = false
  }
}

function applyStartupHash() {
  const hash = window.location.hash || ''
  if (!hash.includes('github-app=')) return
  settingsInitialSection.value = 'integrations'
  activeTab.value = 'settings'
  if (hash.includes('configured')) {
    showToast('GitHub App connected. Install it on repositories before creating a project.')
  } else if (hash.includes('installed')) {
    showToast('GitHub App installation updated. Refresh installations to load repositories.')
  } else if (hash.includes('error')) {
    showToast('GitHub App setup did not complete.')
  }
}

function openGithubAppSettings() {
  settingsInitialSection.value = 'integrations'
  activeTab.value = 'settings'
  projectDialog.open = false
}

async function handleSSEEvent() {
  await refreshTasks()
  await refreshProjects()
  if (selectedTaskId.value) await refreshDetail()
  if (selectedProjectId.value) await refreshProjectDetail()
}

function openCreateTaskDialog() {
  if (!providersState.providers.length) {
    activeTab.value = 'providers'
    showToast('Add a provider before creating a task.')
    return
  }
  taskDialog.open = true
  taskDialog.message = ''
  taskDialog.docker_image = ''
  taskDialog.error = ''
}

async function onCreateTask() {
  if (taskDialog.submitting) return
  taskDialog.error = ''
  taskDialog.submitting = true
  try {
    await createTask(taskDialog.message, taskDialog.docker_image)
    taskDialog.open = false
    activeTab.value = 'tasks'
  } catch (error) {
    taskDialog.error = error.message
  } finally {
    taskDialog.submitting = false
  }
}

async function onSendMessage(payload) {
  const message = typeof payload === 'string' ? payload : payload?.message
  const skillMentions = typeof payload === 'string' ? selectedSkills.value : (payload?.skillMentions || [])
  try {
    messageDraft.value = ''
    await sendTaskMessage(message, skillMentions)
    selectedSkills.value = []
    await loadSkills()
  } catch (error) {
    showToast(error.message)
  }
}

async function onSendProjectMessage(payload) {
  const message = typeof payload === 'string' ? payload : payload?.message
  const skillMentions = typeof payload === 'string' ? selectedProjectSkills.value : (payload?.skillMentions || [])
  try {
    projectMessageDraft.value = ''
    await sendProjectMessage(message, skillMentions)
    selectedProjectSkills.value = []
    await loadSkills()
  } catch (error) {
    showToast(error.message)
  }
}

async function onCreateProjectSession() {
  try {
    await createProjectSession()
  } catch (error) {
    showToast(error.message)
  }
}

async function onApprovePlan() {
  try {
    await approveTaskPlan()
    showToast('Plan approved. Executor and reviewer workflow started.')
  } catch (error) {
    showToast(error.message)
  }
}

async function onRequestPlanRevision(feedback) {
  try {
    await requestPlanRevision(feedback)
    showToast('Revision requested. The planner will update the plan.')
  } catch (error) {
    showToast(error.message)
  }
}

async function onUpdateAgentModel(payload) {
  isUpdatingAgentModel.value = true
  try {
    await updateAgent(selectedTaskDetail.value.selected_agent.id, payload.provider_id, payload.model, payload.reasoning_effort)
    showToast('Agent model updated.')
  } catch (error) {
    showToast(error.message)
  } finally {
    isUpdatingAgentModel.value = false
  }
}

async function onUpdateProjectAgentModel(payload) {
  const agentId = selectedProjectDetail.value?.maintainer_agent?.id
  if (!agentId) return
  isUpdatingProjectAgentModel.value = true
  try {
    await updateProjectAgent(agentId, payload.provider_id, payload.model, payload.reasoning_effort)
    showToast('Agent model updated.')
  } catch (error) {
    showToast(error.message)
  } finally {
    isUpdatingProjectAgentModel.value = false
  }
}

async function onCancelSelectedAgent(id) {
  try {
    await cancelTaskAgent(id)
  } catch (error) {
    showToast(error.message)
  }
}

async function onCancelProjectAgent(id) {
  try {
    await cancelProjectAgent(id)
  } catch (error) {
    showToast(error.message)
  }
}

async function openCreateProjectDialog() {
  resetProjectDialog()
  projectDialog.open = true
  await loadGithubSettings()
  if (!githubSettingsState.app_id || !githubSettingsState.has_private_key) {
    projectDialog.error = 'Configure GitHub App before creating a project.'
    return
  }
  await onRefreshProjectInstallations()
}

function resetProjectDialog() {
  projectDialog.form.name = ''
  projectDialog.form.installation_id = ''
  projectDialog.form.repository_id = ''
  projectDialog.form.owner = ''
  projectDialog.form.repo = ''
  projectDialog.form.docker_image = ''
  projectDialog.repositories = []
  projectDialog.error = ''
  projectDialog.submitting = false
}

async function onRefreshProjectInstallations() {
  projectDialog.loadingInstallations = true
  projectDialog.error = ''
  try {
    await loadGithubSettings()
    const response = await refreshGithubInstallations()
    projectDialog.installations = response?.installations || []
  } catch (error) {
    projectDialog.error = error.message
  } finally {
    projectDialog.loadingInstallations = false
  }
}

async function onLoadProjectInstallations() {
  projectDialog.loadingInstallations = true
  projectDialog.error = ''
  try {
    await loadGithubSettings()
    const response = await loadGithubInstallations()
    projectDialog.installations = response?.installations || []
  } catch (error) {
    projectDialog.error = error.message
  } finally {
    projectDialog.loadingInstallations = false
  }
}

async function onRefreshProjectRepositories() {
  if (!projectDialog.form.installation_id) return
  projectDialog.loadingRepositories = true
  projectDialog.error = ''
  try {
    const response = await loadGithubRepositories(projectDialog.form.installation_id)
    projectDialog.repositories = response?.repositories || []
  } catch (error) {
    projectDialog.error = error.message
  } finally {
    projectDialog.loadingRepositories = false
  }
}

async function onCreateProject() {
  if (projectDialog.submitting) return
  projectDialog.error = ''
  if (!projectDialog.form.owner || !projectDialog.form.repo) {
    projectDialog.error = 'Select a repository before creating the project.'
    return
  }
  projectDialog.submitting = true
  try {
    await createProject({
      name: projectDialog.form.name,
      installation_id: projectDialog.form.installation_id,
      repository_id: projectDialog.form.repository_id,
      owner: projectDialog.form.owner,
      repo: projectDialog.form.repo,
      docker_image: projectDialog.form.docker_image || null
    })
    projectDialog.open = false
    activeTab.value = 'projects'
  } catch (error) {
    projectDialog.error = error.message
  } finally {
    projectDialog.submitting = false
  }
}

async function onSaveAgentConfig(config) {
  try {
    await saveAgentConfig(config)
    showToast('Role config saved.')
  } catch (error) {
    showToast(error.message)
  }
}

async function onLoadSkills() {
  skillsError.value = ''
  try {
    await ensureSkillsLoaded()
  } catch (error) {
    skillsError.value = error.message
    showToast(error.message)
  }
}

async function onSaveSkillsConfig(config) {
  skillsError.value = ''
  try {
    await saveSkillsConfig(config)
    showToast('Skills config saved.')
  } catch (error) {
    skillsError.value = error.message
    showToast(error.message)
  }
}

async function onLoadMcpServers() {
  try {
    await loadMcpServers()
  } catch (error) {
    showToast(error.message)
  }
}

async function onSaveMcpServers(servers) {
  try {
    await saveMcpServers(servers)
    mcpDialogOpen.value = false
    showToast('MCP config saved.')
  } catch (error) {
    showToast(error.message)
  }
}

async function onSaveGithubSettings(token) {
  try {
    await saveGithubSettings(token)
    await loadMcpServers()
    showToast(token ? 'GitHub token saved.' : 'GitHub token cleared.')
  } catch (error) {
    showToast(error.message)
  }
}

async function onSaveGithubAppSettings(payload) {
  try {
    await saveGithubAppSettings(payload)
    showToast('GitHub App settings saved.')
  } catch (error) {
    showToast(error.message)
  }
}

async function onConfigureGithubApp(payload) {
  try {
    await startGithubAppManifest({
      ...payload,
      origin: window.location.origin
    })
  } catch (error) {
    showToast(error.message)
  }
}

async function onRefreshGithubAppInstallations() {
  try {
    await refreshGithubInstallations()
    await loadGithubSettings()
    showToast('GitHub installations refreshed.')
  } catch (error) {
    showToast(error.message)
  }
}

function openGithubAppInstallation() {
  if (!githubSettingsState.install_url) {
    showToast('Configure GitHub App before installing it.')
    return
  }
  window.location.href = githubSettingsState.install_url
}

function confirmCancelTask(id) {
  confirmDialog.title = 'Cancel Task'
  confirmDialog.message = 'Cancel this task and all running task agents?'
  confirmDialog.onConfirm = async () => {
    confirmDialog.open = false
    try {
      await cancelTask(id)
    } catch (error) {
      showToast(error.message)
    }
  }
  confirmDialog.open = true
}

function confirmDeleteTask(id, title) {
  confirmDialog.title = 'Delete Task'
  confirmDialog.message = `Delete "${title || id}" and all task agents? This action cannot be undone.`
  confirmDialog.onConfirm = async () => {
    confirmDialog.open = false
    try {
      await deleteTask(id)
    } catch (error) {
      showToast(error.message)
    }
  }
  confirmDialog.open = true
}

function confirmDeleteTaskAgent() {
  showToast('Delete the task to remove task-owned agents.')
}

function confirmDeleteProjectAgent() {
  showToast('Delete the project to remove project-owned agents.')
}

function confirmDeleteProvider(index, name) {
  confirmDialog.title = 'Delete Provider'
  confirmDialog.message = `Are you sure you want to delete "${name}"? Task agents using this provider may stop working.`
  confirmDialog.onConfirm = async () => {
    confirmDialog.open = false
    try {
      await removeProvider(index)
    } catch (error) {
      showToast(error.message)
    }
  }
  confirmDialog.open = true
}

function onConfirmAction() {
  if (confirmDialog.onConfirm) confirmDialog.onConfirm()
}
</script>
