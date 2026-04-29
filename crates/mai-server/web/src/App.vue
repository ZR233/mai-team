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
          v-model:conversation-ref="conversationRef"
          @cancel="cancelAgent"
          @delete="confirmDeleteAgent"
          @send="onSendMessage"
          @update:draft="messageDraft = $event"
        />
      </section>

      <ProviderGrid
        v-else
        :providers="providersState.providers"
        :default-id="providersState.default_provider_id"
        @add="openProviderDialog(null)"
        @edit="openProviderDialog"
        @delete="confirmDeleteProvider"
      />
    </main>

    <ProviderDialog
      :dialog="providerDialog"
      @close="closeProviderDialog"
      @save="saveProviderDialog"
    />

    <AgentDialog
      :dialog="agentDialog"
      :providers="providersState.providers"
      :models="selectedProviderModels"
      @close="agentDialog.open = false"
      @create="onCreateAgent"
      @provider-changed="onAgentProviderChanged"
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

import { useApi } from './composables/useApi'
import { useSSE } from './composables/useSSE'
import { useAgents } from './composables/useAgents'
import { useProviders } from './composables/useProviders'

const { toast, showToast } = useApi()
const { eventFeed, connectionState, connectEvents, disconnect } = useSSE()
const {
  agents, selectedAgentId, selectedDetail, isLoading, isSending, isDetailLoading,
  conversationRef, agentDialog,
  refreshAgents, refreshDetail, selectAgent, createAgent, sendMessage, cancelAgent, deleteAgent,
  scrollConversationToBottom
} = useAgents()
const {
  providersState, providerDialog,
  loadProviders, removeProvider, openProviderDialog, closeProviderDialog, saveProviderDialog
} = useProviders()

const activeTab = ref('agents')
const messageDraft = ref('')

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

const selectedProviderModels = computed(() => {
  const provider = providersState.providers.find((p) => p.id === agentDialog.provider_id)
  return provider?.models || []
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
    await Promise.all([loadProviders(), refreshAgents()])
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
  agentDialog.model = defaultProvider?.default_model || defaultProvider?.models?.[0] || ''
  agentDialog.error = ''
}

function onAgentProviderChanged() {
  const provider = providersState.providers.find((p) => p.id === agentDialog.provider_id)
  agentDialog.model = provider?.default_model || provider?.models?.[0] || ''
}

async function onCreateAgent() {
  agentDialog.error = ''
  try {
    await createAgent(agentDialog.name, agentDialog.provider_id, agentDialog.model)
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

function confirmDeleteAgent(id, name) {
  confirmDialog.title = 'Delete Agent'
  confirmDialog.message = `Are you sure you want to delete "${name || id}"? This action cannot be undone.`
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
