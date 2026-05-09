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
        :git-accounts-state="gitAccountsState"
        :initial-section="settingsInitialSection"
        @reload="loadAgentConfig"
        @save="onSaveAgentConfig"
        @reload-skills="onLoadSkills"
        @save-skills="onSaveSkillsConfig"
        @reload-mcp="onLoadMcpServers"
        @open-mcp="mcpDialogOpen = true"
        @open-providers="activeTab = 'providers'"
        @save-git-account="onSaveGitAccount"
        @verify-git-account="onVerifyGitAccount"
        @delete-git-account="onDeleteGitAccount"
        @set-default-git-account="onSetDefaultGitAccount"
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
      @close="projectDialog.open = false"
      @create="onCreateProject"
      @configure-git-accounts="openGitAccountsSettings"
      @refresh-repositories="onRefreshProjectRepositories"
      @load-repository-packages="onLoadProjectRepositoryPackages"
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
import { useGitAccounts } from './composables/useGitAccounts'

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
  loadGitAccountRepositories,
  loadRuntimeDefaults,
  loadGitAccountRepositoryPackages,
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
  gitAccountsState,
  loadGitAccounts,
  saveGitAccount,
  verifyGitAccount,
  deleteGitAccount,
  setDefaultGitAccount
} = useGitAccounts()

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
    await Promise.all([loadProviders(), loadAgentConfig(), loadSkills(), loadMcpServers(), loadGitAccounts(), refreshTasks(), refreshProjects()])
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
  if (!hash.includes('settings=git-accounts')) return
  settingsInitialSection.value = 'git-accounts'
  activeTab.value = 'settings'
}

function openGitAccountsSettings() {
  settingsInitialSection.value = 'git-accounts'
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
  await loadProjectRuntimeDefaults()
  await loadGitAccounts()
  projectDialog.gitAccounts = gitAccountsState.accounts || []
  const defaultAccount = projectDialog.gitAccounts.find((account) => account.is_default) || projectDialog.gitAccounts[0]
  if (!defaultAccount) {
    projectDialog.error = 'Add a Git account before creating a project.'
    return
  }
  projectDialog.form.git_account_id = defaultAccount.id
  await onRefreshProjectRepositories()
}

function resetProjectDialog() {
  projectDialog.mode = 'git_account'
  projectDialog.form.name = ''
  projectDialog.form.git_account_id = ''
  projectDialog.form.repository_full_name = ''
  projectDialog.form.branch = ''
  projectDialog.repository.query = ''
  projectDialog.runtime.docker_image = ''
  projectDialog.runtime.default_docker_image = ''
  projectDialog.runtime.packages = []
  projectDialog.runtime.package_image = ''
  projectDialog.runtime.loadingPackages = false
  projectDialog.runtime.packageWarning = ''
  projectDialog.gitAccounts = []
  projectDialog.repositories = []
  projectDialog.error = ''
  projectDialog.submitting = false
}

async function loadProjectRuntimeDefaults() {
  try {
    const defaults = await loadRuntimeDefaults()
    projectDialog.runtime.default_docker_image = defaults?.default_docker_image || ''
    projectDialog.runtime.docker_image = projectDialog.runtime.default_docker_image
  } catch (error) {
    projectDialog.error = error.message
  }
}

async function onRefreshProjectRepositories() {
  if (!projectDialog.form.git_account_id) return
  projectDialog.loadingRepositories = true
  projectDialog.error = ''
  clearProjectRepositoryPackages()
  try {
    const response = await loadGitAccountRepositories(projectDialog.form.git_account_id)
    projectDialog.repositories = response?.repositories || []
    if (projectDialog.form.repository_full_name && !projectDialog.repositories.some((repository) => {
      return repository.full_name === projectDialog.form.repository_full_name
    })) {
      projectDialog.form.repository_full_name = ''
      projectDialog.form.branch = ''
    }
    if (!projectDialog.form.repository_full_name && projectDialog.repositories.length === 1) {
      const repository = projectDialog.repositories[0]
      projectDialog.form.repository_full_name = repository.full_name
      projectDialog.form.branch = repository.default_branch || 'main'
      if (!projectDialog.form.name.trim()) {
        projectDialog.form.name = repository.full_name || [repository.owner, repository.name].filter(Boolean).join('/')
      }
      await onLoadProjectRepositoryPackages()
    }
  } catch (error) {
    projectDialog.error = error.message
  } finally {
    projectDialog.loadingRepositories = false
  }
}

function clearProjectRepositoryPackages() {
  projectDialog.runtime.packages = []
  projectDialog.runtime.package_image = ''
  projectDialog.runtime.packageWarning = ''
}

async function onLoadProjectRepositoryPackages() {
  if (!projectDialog.form.git_account_id || !projectDialog.form.repository_full_name) {
    clearProjectRepositoryPackages()
    return
  }
  projectDialog.runtime.loadingPackages = true
  projectDialog.runtime.packageWarning = ''
  try {
    const response = await loadGitAccountRepositoryPackages(
      projectDialog.form.git_account_id,
      projectDialog.form.repository_full_name
    )
    projectDialog.runtime.packages = response?.packages || []
    projectDialog.runtime.package_image = ''
    projectDialog.runtime.packageWarning = response?.warning || ''
  } catch (error) {
    projectDialog.runtime.packages = []
    projectDialog.runtime.package_image = ''
    projectDialog.runtime.packageWarning = error.message
  } finally {
    projectDialog.runtime.loadingPackages = false
  }
}

async function onCreateProject() {
  if (projectDialog.submitting) return
  projectDialog.error = ''
  if (!projectDialog.form.repository_full_name) {
    projectDialog.error = 'Select a repository before creating the project.'
    return
  }
  if (!projectDialog.form.git_account_id) {
    projectDialog.error = 'Select a Git account before creating the project.'
    return
  }
  projectDialog.submitting = true
  try {
    await createProject({
      name: projectDialog.form.name,
      git_account_id: projectDialog.form.git_account_id,
      repository_full_name: projectDialog.form.repository_full_name,
      branch: projectDialog.form.branch || null,
      docker_image: projectDialog.runtime.docker_image || null
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

async function onSaveGitAccount(payload) {
  try {
    await saveGitAccount(payload)
    showToast('Git account saved. Verifying connection...')
  } catch (error) {
    showToast(error.message)
  }
}

async function onVerifyGitAccount(id) {
  try {
    await verifyGitAccount(id)
    showToast('Git account verified.')
  } catch (error) {
    showToast(error.message)
  }
}

async function onDeleteGitAccount(id) {
  try {
    await deleteGitAccount(id)
    showToast('Git account deleted.')
  } catch (error) {
    showToast(error.message)
  }
}

async function onSetDefaultGitAccount(id) {
  try {
    await setDefaultGitAccount(id)
    showToast('Default Git account updated.')
  } catch (error) {
    showToast(error.message)
  }
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
