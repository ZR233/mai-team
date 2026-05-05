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

      <section v-if="activeTab === 'tasks'" class="agent-stage">
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
          :updating-model="isUpdatingAgentModel"
          v-model:conversation-ref="conversationRef"
          @approve-plan="onApprovePlan"
          @cancel="confirmCancelTask"
          @cancel-agent="onCancelSelectedAgent"
          @delete="confirmDeleteTask"
          @delete-agent="confirmDeleteTaskAgent"
          @send="onSendMessage"
          @update-model="onUpdateAgentModel"
          @update:draft="messageDraft = $event"
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
        :loading="agentConfigState.loading"
        :saving="agentConfigState.saving"
        @reload="loadAgentConfig"
        @save="onSaveAgentConfig"
        @open-providers="activeTab = 'providers'"
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
import ProviderGrid from './components/ProviderGrid.vue'
import ConfirmDialog from './components/ConfirmDialog.vue'
import ProviderDialog from './components/ProviderDialog.vue'
import TaskDialog from './components/TaskDialog.vue'
import ResearchAgentConfigPanel from './components/ResearchAgentConfigPanel.vue'

import { useApi } from './composables/useApi'
import { useSSE } from './composables/useSSE'
import { useTasks } from './composables/useTasks'
import { useProviders } from './composables/useProviders'
import { useAgentConfig } from './composables/useAgentConfig'

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
  cancelTask,
  cancelTaskAgent,
  deleteTask,
  updateAgent,
  scrollConversationToBottom
} = useTasks()
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

const activeTab = ref('tasks')
const messageDraft = ref('')
const isUpdatingAgentModel = ref(false)

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

onMounted(async () => {
  connectEvents(handleSSEEvent)
  await refreshAll()
})

onUnmounted(() => disconnect())

async function refreshAll() {
  isLoading.value = true
  try {
    await Promise.all([loadProviders(), loadAgentConfig(), refreshTasks()])
    if (providersState.providers.length && !tasks.value.length) {
      await ensureDefaultTask()
    } else if (selectedTaskId.value) {
      await refreshDetail()
    } else if (tasks.value[0]?.id) {
      await selectTask(tasks.value[0].id)
    }
  } catch (error) {
    showToast(error.message)
  } finally {
    isLoading.value = false
  }
}

async function handleSSEEvent() {
  await refreshTasks()
  if (selectedTaskId.value) await refreshDetail()
}

function openCreateTaskDialog() {
  if (!providersState.providers.length) {
    activeTab.value = 'providers'
    showToast('Add a provider before creating a task.')
    return
  }
  taskDialog.open = true
  taskDialog.title = ''
  taskDialog.message = ''
  taskDialog.docker_image = ''
  taskDialog.error = ''
}

async function onCreateTask() {
  taskDialog.error = ''
  try {
    await createTask(taskDialog.title, taskDialog.message, taskDialog.docker_image)
    taskDialog.open = false
    activeTab.value = 'tasks'
  } catch (error) {
    taskDialog.error = error.message
  }
}

async function onSendMessage(message) {
  try {
    messageDraft.value = ''
    await sendTaskMessage(message)
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

async function onCancelSelectedAgent(id) {
  try {
    await cancelTaskAgent(id)
  } catch (error) {
    showToast(error.message)
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
