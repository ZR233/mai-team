<template>
  <div class="app-shell">
    <header class="topbar">
      <div class="brand">
        <div class="brand-mark">M</div>
        <div>
          <h1>Mai Team</h1>
          <p>Multi-agent runtime console</p>
        </div>
      </div>
      <nav class="tabs" aria-label="Views">
        <button :class="{ active: activeTab === 'agents' }" @click="activeTab = 'agents'">Agents</button>
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
      <AgentRail
        :agents="agents"
        :selected-id="selectedAgentId"
        :visible="activeTab === 'agents'"
        @select="selectAgent"
        @create="openCreateAgentDialog"
      />

      <section v-if="activeTab === 'agents'" class="agent-stage">
        <div v-if="!selectedDetail" class="empty-stage">
          <div class="empty-mark">+</div>
          <h2>No agent selected</h2>
          <p>Create a new agent or choose one from the left rail.</p>
          <button class="primary-button" @click="openCreateAgentDialog">Create Agent</button>
        </div>

        <AgentDetail
          v-else
          :detail="selectedDetail"
          :events="eventFeed"
          :draft="messageDraft"
          :loading="isDetailLoading"
          :sending="isSending"
          :providers="providersState.providers"
          :updating-model="isUpdatingAgentModel"
          v-model:conversation-ref="conversationRef"
          @cancel="cancelAgent"
          @create-session="onCreateSession"
          @delete="confirmDeleteAgent"
          @select-session="onSelectSession"
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

    <AgentDialog
      :dialog="agentDialog"
      :providers="providersState.providers"
      @close="agentDialog.open = false"
      @create="onCreateAgent"
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

import AgentRail from './components/AgentRail.vue'
import AgentDetail from './components/AgentDetail.vue'
import ProviderGrid from './components/ProviderGrid.vue'
import ConfirmDialog from './components/ConfirmDialog.vue'
import ProviderDialog from './components/ProviderDialog.vue'
import AgentDialog from './components/AgentDialog.vue'
import ResearchAgentConfigPanel from './components/ResearchAgentConfigPanel.vue'

import { useApi } from './composables/useApi'
import { useSSE } from './composables/useSSE'
import { useAgents } from './composables/useAgents'
import { useProviders } from './composables/useProviders'
import { useAgentConfig } from './composables/useAgentConfig'
import { defaultReasoningEffort } from './utils/reasoning'
import { countAgentDescendants } from './utils/agentTree'

const { toast, showToast } = useApi()
const { eventFeed, connectionState, connectEvents, disconnect } = useSSE()
const {
  agents, selectedAgentId, selectedDetail, isLoading, isSending, isDetailLoading,
  conversationRef, agentDialog,
  refreshAgents, refreshDetail, selectAgent, selectSession, createAgent, createSession,
  sendMessage, cancelAgent, deleteAgent,
  updateAgent, scrollConversationToBottom
} = useAgents()
const {
  providersState, providerDialog,
  loadProviders, removeProvider, openProviderDialog, closeProviderDialog, saveProviderDialog,
  fillFromPreset
} = useProviders()
const { agentConfigState, loadAgentConfig, saveAgentConfig } = useAgentConfig()

const activeTab = ref('agents')
const messageDraft = ref('')
const isUpdatingAgentModel = ref(false)

// Confirm dialog
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
    selectedDetail.value?.messages?.length,
    selectedDetail.value?.recent_events?.length,
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
    await Promise.all([loadProviders(), loadAgentConfig(), refreshAgents()])
    if (selectedAgentId.value) await refreshDetail()
  } catch (error) {
    showToast(error.message)
  } finally {
    isLoading.value = false
  }
}

async function handleSSEEvent() {
  await refreshAgents()
  if (selectedAgentId.value) await refreshDetail()
}

function openCreateAgentDialog() {
  if (!providersState.providers.length) {
    activeTab.value = 'providers'
    showToast('Add a provider before creating an agent.')
    return
  }
  const defaultProvider =
    providersState.providers.find((p) => p.id === providersState.default_provider_id) ||
    providersState.providers[0]
  agentDialog.open = true
  agentDialog.name = ''
  agentDialog.provider_id = defaultProvider?.id || ''
  agentDialog.model = defaultProvider?.default_model || defaultProvider?.models?.[0]?.id || ''
  resetAgentReasoningEffort()
  agentDialog.error = ''
}

function resetAgentReasoningEffort() {
  const provider = providersState.providers.find((p) => p.id === agentDialog.provider_id)
  const model = provider?.models?.find((item) => item.id === agentDialog.model)
  agentDialog.reasoning_effort = defaultReasoningEffort(provider, model)
}

async function onCreateAgent() {
  agentDialog.error = ''
  try {
    await createAgent(agentDialog.name, agentDialog.provider_id, agentDialog.model, agentDialog.reasoning_effort)
    agentDialog.open = false
    activeTab.value = 'agents'
  } catch (error) {
    agentDialog.error = error.message
  }
}

async function onSendMessage(message) {
  try {
    messageDraft.value = ''
    await sendMessage(message)
  } catch (error) {
    showToast(error.message)
  }
}

async function onCreateSession() {
  try {
    messageDraft.value = ''
    await createSession()
  } catch (error) {
    showToast(error.message)
  }
}

async function onSelectSession(sessionId) {
  try {
    messageDraft.value = ''
    await selectSession(sessionId)
  } catch (error) {
    showToast(error.message)
  }
}

async function onUpdateAgentModel(payload) {
  isUpdatingAgentModel.value = true
  try {
    await updateAgent(selectedDetail.value.id, payload.provider_id, payload.model, payload.reasoning_effort)
    showToast('Agent model updated.')
  } catch (error) {
    showToast(error.message)
  } finally {
    isUpdatingAgentModel.value = false
  }
}

async function onSaveAgentConfig(researchAgent) {
  try {
    await saveAgentConfig(researchAgent)
    showToast('Agent config saved.')
  } catch (error) {
    showToast(error.message)
  }
}

function confirmDeleteAgent(id, name) {
  const descendants = countAgentDescendants(id, agents.value)
  confirmDialog.title = 'Delete Agent'
  confirmDialog.message = descendants
    ? `Are you sure you want to delete "${name || id}" and ${descendants} subagent${descendants === 1 ? '' : 's'}? This action cannot be undone.`
    : `Are you sure you want to delete "${name || id}"? This action cannot be undone.`
  confirmDialog.onConfirm = async () => {
    confirmDialog.open = false
    try {
      await deleteAgent(id)
    } catch (error) {
      showToast(error.message)
    }
  }
  confirmDialog.open = true
}

function confirmDeleteProvider(index, name) {
  confirmDialog.title = 'Delete Provider'
  confirmDialog.message = `Are you sure you want to delete "${name}"? Agents using this provider may stop working.`
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
