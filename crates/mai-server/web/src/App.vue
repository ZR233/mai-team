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
        <button class="ghost-button" @click="openTokenDialog('Bearer Token')">Token</button>
        <button class="primary-button" :disabled="isLoading" @click="refreshAll">
          <span v-if="isLoading" class="spinner-sm"></span>
          <template v-else>Refresh</template>
        </button>
      </div>
    </header>

    <main class="workspace">
      <aside class="agent-rail" :class="{ hidden: activeTab !== 'agents' }">
        <div class="rail-head">
          <div>
            <h2>Agents</h2>
            <p>{{ agents.length }} active session{{ agents.length === 1 ? '' : 's' }}</p>
          </div>
          <button class="small-button" @click="openCreateAgentDialog">New</button>
        </div>
        <div class="agent-list">
          <button
            v-for="agent in agents"
            :key="agent.id"
            class="agent-row"
            :class="{ active: selectedAgentId === agent.id }"
            @click="selectAgent(agent.id)"
          >
            <span class="avatar">{{ initial(agent.name) }}</span>
            <span class="agent-row-main">
              <span class="agent-name">{{ agent.name }}</span>
              <span class="agent-meta">{{ agent.provider_name }} / {{ agent.model }}</span>
            </span>
            <span class="status-dot" :class="statusTone(agent.status)" :title="formatStatus(agent.status)" />
          </button>
          <div v-if="!agents.length" class="empty-rail">
            <strong>No agents yet</strong>
            <span>Create one to start a container-backed session.</span>
          </div>
        </div>
      </aside>

      <section v-if="activeTab === 'agents'" class="agent-stage">
        <div v-if="!selectedDetail" class="empty-stage">
          <div class="empty-mark">+</div>
          <h2>No agent selected</h2>
          <p>Create a new agent or choose one from the left rail.</p>
          <button class="primary-button" @click="openCreateAgentDialog">Create Agent</button>
        </div>

        <template v-else>
          <div class="agent-header">
            <div class="large-avatar">{{ initial(selectedDetail.name) }}</div>
            <div class="agent-title">
              <div class="title-row">
                <h2>{{ selectedDetail.name }}</h2>
                <span class="status-pill" :class="statusTone(selectedDetail.status)">
                  {{ formatStatus(selectedDetail.status) }}
                </span>
              </div>
              <p>{{ selectedDetail.provider_name }} / {{ selectedDetail.model }}</p>
              <p v-if="selectedDetail.last_error" class="error-text">{{ selectedDetail.last_error }}</p>
            </div>
            <div class="agent-actions">
              <button class="ghost-button" @click="cancelAgent(selectedDetail.id)">Cancel</button>
              <button class="danger-button" @click="confirmDeleteAgent(selectedDetail.id, selectedDetail.name)">Delete</button>
            </div>
          </div>

          <div class="detail-strip">
            <div>
              <span>Status</span>
              <strong>{{ formatStatus(selectedDetail.status) }}</strong>
            </div>
            <div>
              <span>Container</span>
              <strong>{{ shortContainer(selectedDetail.container_id) }}</strong>
            </div>
            <div>
              <span>Provider</span>
              <strong>{{ selectedDetail.provider_id }}</strong>
            </div>
            <div>
              <span>Tokens</span>
              <strong>{{ totalTokens(selectedDetail) }}</strong>
            </div>
            <div>
              <span>Created</span>
              <strong>{{ formatDate(selectedDetail.created_at) }}</strong>
            </div>
          </div>

          <div class="agent-body">
            <section class="conversation" ref="conversationRef">
              <div v-if="isDetailLoading" class="loading-center">
                <div class="spinner"></div>
                <span>Loading messages...</span>
              </div>
              <div v-else-if="!selectedDetail.messages?.length" class="quiet-empty">
                <strong>No messages yet</strong>
                <span>Send the first instruction to start a turn.</span>
              </div>
              <template v-else>
                <article
                  v-for="(message, index) in selectedDetail.messages"
                  :key="`${message.created_at}-${message.role}-${index}`"
                  class="message"
                  :class="messageClass(message.role)"
                >
                  <div class="message-avatar">{{ roleInitial(message.role) }}</div>
                  <div class="message-content">
                    <span>{{ roleLabel(message.role) }}</span>
                    <div v-if="message.role === 'tool'" class="tool-content">
                      <p>{{ message.content }}</p>
                    </div>
                    <div v-else class="markdown-body" v-html="renderMarkdown(message.content)"></div>
                  </div>
                </article>
              </template>
            </section>

            <aside class="event-panel">
              <div class="panel-head">
                <h3>Recent Events</h3>
                <span class="event-count">{{ eventFeed.length }}</span>
              </div>
              <div class="event-list">
                <div
                  v-for="event in eventFeed"
                  :key="event.sequence"
                  class="event-row"
                  :class="`event-${event.type || 'unknown'}`"
                >
                  <div class="event-row-head">
                    <span class="event-type">{{ formatEventType(event.type) }}</span>
                    <time>{{ formatTime(event.timestamp) }}</time>
                  </div>
                  <p>{{ eventSummary(event) }}</p>
                </div>
                <div v-if="!eventFeed.length" class="quiet-empty compact">
                  <strong>No events yet</strong>
                  <span>Lifecycle and tool events appear here.</span>
                </div>
              </div>
            </aside>
          </div>

          <form class="composer" @submit.prevent="sendMessage">
            <textarea
              v-model="messageDraft"
              rows="2"
              placeholder="Send a command or message... (Enter to send, Shift+Enter for new line)"
              @keydown.enter.exact="handleEnterKey"
            />
            <button class="primary-button" type="submit" :disabled="!messageDraft.trim() || isSending">
              <span v-if="isSending" class="spinner-sm"></span>
              <template v-else>Send</template>
            </button>
          </form>
        </template>
      </section>

      <section v-else class="providers-stage">
        <div class="stage-title">
          <div>
            <h2>Providers</h2>
            <p>Configure OpenAI-compatible endpoints and the model list offered to new agents.</p>
          </div>
          <button class="primary-button" @click="openProviderDialog(null)">Add Provider</button>
        </div>

        <div class="provider-grid">
          <article
            v-for="(provider, index) in providersState.providers"
            :key="provider.id"
            class="provider-card"
            :class="{ disabled: !provider.enabled }"
          >
            <div class="provider-main">
              <div class="provider-icon">{{ initial(provider.name) }}</div>
              <div>
                <div class="provider-name-line">
                  <h3>{{ provider.name }}</h3>
                  <span v-if="providersState.default_provider_id === provider.id" class="mini-pill">Default</span>
                  <span class="mini-pill" :class="provider.enabled ? 'green' : 'amber'">
                    {{ provider.enabled ? 'Enabled' : 'Disabled' }}
                  </span>
                </div>
                <p class="mono">{{ provider.id }}</p>
              </div>
            </div>
            <div class="provider-url">{{ provider.base_url }}</div>
            <div class="provider-stats">
              <span :class="provider.has_api_key ? 'good' : 'bad'">
                {{ provider.has_api_key ? 'API key set' : 'No API key' }}
              </span>
              <span>{{ provider.models.length }} model{{ provider.models.length === 1 ? '' : 's' }}</span>
              <span>Default model: {{ provider.default_model }}</span>
            </div>
            <div class="model-chips">
              <span
                v-for="model in provider.models"
                :key="model"
                class="model-chip"
                :class="{ default: model === provider.default_model }"
              >
                {{ model }}
              </span>
              <span v-if="!provider.models.length" class="model-chip muted">No models</span>
            </div>
            <div class="provider-actions">
              <button class="ghost-button" @click="openProviderDialog(index)">Edit</button>
              <button class="danger-button" @click="confirmDeleteProvider(index, provider.name)">Delete</button>
            </div>
          </article>
        </div>

        <div v-if="!providersState.providers.length" class="empty-stage providers-empty">
          <div class="empty-mark">P</div>
          <h2>No providers configured</h2>
          <p>Add a provider before creating agents.</p>
          <button class="primary-button" @click="openProviderDialog(null)">Add Provider</button>
        </div>
      </section>
    </main>

    <!-- Token Dialog -->
    <div v-if="tokenDialog.open" class="modal-backdrop" @mousedown.self="onBackdropDown" @mouseup.self="onBackdropUp($event, () => tokenDialog.open = false)">
      <form class="modal" @submit.prevent="saveToken">
        <h2>{{ tokenDialog.title }}</h2>
        <label>
          <span>Bearer Token</span>
          <input v-model="tokenDialog.value" autocomplete="off" placeholder="Paste token printed by server" />
        </label>
        <div class="modal-actions">
          <button class="ghost-button" type="button" @click="tokenDialog.open = false">Cancel</button>
          <button class="primary-button" type="submit">Save Token</button>
        </div>
      </form>
    </div>

    <!-- Provider Dialog -->
    <div v-if="providerDialog.open" class="modal-backdrop" @mousedown.self="onBackdropDown" @mouseup.self="onBackdropUp($event, closeProviderDialog)">
      <form class="modal wide" @submit.prevent="saveProviderDialog">
        <div class="modal-title-row">
          <h2>{{ providerDialog.index === null ? 'Add Provider' : 'Edit Provider' }}</h2>
          <button class="icon-button" type="button" @click="closeProviderDialog">Close</button>
        </div>
        <div class="form-grid">
          <label>
            <span>Provider ID</span>
            <input v-model.trim="providerDialog.form.id" placeholder="openai" />
          </label>
          <label>
            <span>Display Name</span>
            <input v-model.trim="providerDialog.form.name" placeholder="OpenAI" />
          </label>
          <label class="span-2">
            <span>OpenAI Base URL</span>
            <input v-model.trim="providerDialog.form.base_url" placeholder="https://api.openai.com/v1" />
          </label>
          <label>
            <span>API Key</span>
            <input
              v-model="providerDialog.form.api_key"
              type="password"
              :placeholder="providerDialog.form.has_api_key ? 'Leave blank to keep existing key' : 'sk-...'"
            />
          </label>
          <label>
            <span>Default Model</span>
            <input v-model.trim="providerDialog.form.default_model" placeholder="gpt-5.2" />
          </label>
          <label class="span-2">
            <span>Models</span>
            <textarea v-model="providerDialog.form.modelsText" rows="5" placeholder="One model per line" />
          </label>
          <label>
            <span>Enabled</span>
            <select v-model="providerDialog.form.enabled">
              <option :value="true">Enabled</option>
              <option :value="false">Disabled</option>
            </select>
          </label>
          <label>
            <span>Default Provider</span>
            <select v-model="providerDialog.form.default_provider">
              <option :value="false">No</option>
              <option :value="true">Yes</option>
            </select>
          </label>
        </div>
        <p v-if="providerDialog.error" class="dialog-error">{{ providerDialog.error }}</p>
        <div class="modal-actions">
          <button class="ghost-button" type="button" @click="closeProviderDialog">Cancel</button>
          <button class="primary-button" type="submit">Save Provider</button>
        </div>
      </form>
    </div>

    <!-- Agent Dialog -->
    <div v-if="agentDialog.open" class="modal-backdrop" @mousedown.self="onBackdropDown" @mouseup.self="onBackdropUp($event, () => agentDialog.open = false)">
      <form class="modal" @submit.prevent="createAgent">
        <div class="modal-title-row">
          <h2>Create Agent</h2>
          <button class="icon-button" type="button" @click="agentDialog.open = false">Close</button>
        </div>
        <label>
          <span>Agent Name</span>
          <input v-model.trim="agentDialog.name" placeholder="Research Agent" />
        </label>
        <label>
          <span>Provider</span>
          <select v-model="agentDialog.provider_id">
            <option v-for="provider in providersState.providers" :key="provider.id" :value="provider.id">
              {{ provider.name }}
            </option>
          </select>
        </label>
        <label>
          <span>Model</span>
          <select v-model="agentDialog.model">
            <option v-for="model in selectedProviderModels" :key="model" :value="model">{{ model }}</option>
          </select>
        </label>
        <p v-if="agentDialog.error" class="dialog-error">{{ agentDialog.error }}</p>
        <div class="modal-actions">
          <button class="ghost-button" type="button" @click="agentDialog.open = false">Cancel</button>
          <button class="primary-button" type="submit" :disabled="!providersState.providers.length">Create</button>
        </div>
      </form>
    </div>

    <!-- Confirm Dialog -->
    <div v-if="confirmDialog.open" class="modal-backdrop">
      <div class="modal confirm-modal">
        <h2>{{ confirmDialog.title }}</h2>
        <p class="confirm-message">{{ confirmDialog.message }}</p>
        <div class="modal-actions">
          <button class="ghost-button" @click="confirmDialog.open = false">Cancel</button>
          <button class="danger-button" @click="confirmDialog.onConfirm">Confirm</button>
        </div>
      </div>
    </div>

    <Transition name="toast">
      <div v-if="toast" class="toast">{{ toast }}</div>
    </Transition>
  </div>
</template>

<script setup>
import { computed, nextTick, onMounted, onUnmounted, reactive, ref, watch } from 'vue'
import { marked } from 'marked'
import hljs from 'highlight.js'

marked.setOptions({ breaks: true, gfm: true })

const activeTab = ref('agents')
const agents = ref([])
const selectedAgentId = ref(null)
const selectedDetail = ref(null)
const messageDraft = ref('')
const eventFeed = ref([])
const toast = ref('')
const connectionState = ref('offline')
const isLoading = ref(false)
const isSending = ref(false)
const isDetailLoading = ref(false)
const conversationRef = ref(null)

const providersState = reactive({
  providers: [],
  default_provider_id: null
})

let eventSource = null
let token = localStorage.getItem('maiToken') || new URLSearchParams(window.location.search).get('token') || ''
let tokenResolver = null
let sseRetryCount = 0
const SSE_MAX_RETRIES = 5
let sseRetryTimer = null
let backdropDown = false

const confirmDialog = reactive({
  open: false,
  title: '',
  message: '',
  onConfirm: () => {}
})

const tokenDialog = reactive({
  open: false,
  title: 'Bearer Token',
  value: ''
})

const providerDialog = reactive({
  open: false,
  index: null,
  error: '',
  form: emptyProviderForm()
})

const agentDialog = reactive({
  open: false,
  name: '',
  provider_id: '',
  model: '',
  error: ''
})

const connectionLabel = computed(() => {
  if (connectionState.value === 'online') return 'Connected'
  if (connectionState.value === 'connecting') return 'Connecting'
  return 'Offline'
})

const selectedProviderModels = computed(() => {
  const provider = providersState.providers.find((item) => item.id === agentDialog.provider_id)
  return provider?.models || []
})

watch(
  () => agentDialog.provider_id,
  () => {
    const provider = providersState.providers.find((item) => item.id === agentDialog.provider_id)
    agentDialog.model = provider?.default_model || provider?.models?.[0] || ''
  }
)

watch(
  () => selectedDetail.value?.messages?.length,
  async () => {
    await nextTick()
    highlightCodeBlocks()
    scrollToBottom()
  }
)

onMounted(async () => {
  connectEvents()
  await refreshAll()
})

onUnmounted(() => {
  if (eventSource) eventSource.close()
  if (sseRetryTimer) clearTimeout(sseRetryTimer)
})

function renderMarkdown(content) {
  if (!content) return ''
  return marked.parse(String(content))
}

function onBackdropDown() {
  backdropDown = true
}

function onBackdropUp(event, callback) {
  if (backdropDown) {
    backdropDown = false
    callback()
  }
}

function highlightCodeBlocks() {
  if (!conversationRef.value) return
  conversationRef.value.querySelectorAll('pre code').forEach((block) => {
    if (!block.dataset.highlighted) {
      hljs.highlightElement(block)
      block.dataset.highlighted = 'true'
    }
  })
}

async function scrollToBottom() {
  await nextTick()
  if (conversationRef.value) {
    conversationRef.value.scrollTop = conversationRef.value.scrollHeight
  }
}

async function api(path, init = {}, retry = true) {
  if (!token) {
    await openTokenDialog('Bearer Token')
  }
  const headers = {
    authorization: `Bearer ${token}`,
    ...(init.body ? { 'content-type': 'application/json' } : {}),
    ...(init.headers || {})
  }
  const response = await fetch(path, { ...init, headers })
  if (response.status === 401 && retry) {
    await openTokenDialog('Token expired or invalid')
    return api(path, init, false)
  }
  if (!response.ok) {
    const text = await response.text()
    throw new Error(readError(text))
  }
  if (response.status === 204) return null
  return response.json()
}

function readError(text) {
  try {
    return JSON.parse(text).error || text
  } catch {
    return text
  }
}

async function refreshAll() {
  isLoading.value = true
  try {
    await Promise.all([loadProviders(), refreshAgents()])
    if (selectedAgentId.value) {
      await refreshDetail()
    }
  } catch (error) {
    showToast(error.message)
  } finally {
    isLoading.value = false
  }
}

async function loadProviders() {
  const response = await api('/providers')
  providersState.providers = response.providers || []
  providersState.default_provider_id = response.default_provider_id || null
}

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
    highlightCodeBlocks()
    scrollToBottom()
  } finally {
    isDetailLoading.value = false
  }
}

async function selectAgent(id) {
  selectedAgentId.value = id
  await refreshAgents()
  await refreshDetail()
}

function openCreateAgentDialog() {
  if (!providersState.providers.length) {
    activeTab.value = 'providers'
    showToast('Add a provider before creating an agent.')
    return
  }
  const defaultProvider =
    providersState.providers.find((provider) => provider.id === providersState.default_provider_id) ||
    providersState.providers[0]
  agentDialog.open = true
  agentDialog.name = ''
  agentDialog.provider_id = defaultProvider?.id || ''
  agentDialog.model = defaultProvider?.default_model || defaultProvider?.models?.[0] || ''
  agentDialog.error = ''
}

async function createAgent() {
  agentDialog.error = ''
  try {
    const response = await api('/agents', {
      method: 'POST',
      body: JSON.stringify({
        name: agentDialog.name || null,
        provider_id: agentDialog.provider_id || null,
        model: agentDialog.model || null
      })
    })
    selectedAgentId.value = response.agent.id
    agentDialog.open = false
    activeTab.value = 'agents'
    await refreshAgents()
    await refreshDetail()
  } catch (error) {
    agentDialog.error = error.message
  }
}

async function sendMessage() {
  const message = messageDraft.value.trim()
  if (!selectedAgentId.value || !message) return
  isSending.value = true
  try {
    await api(`/agents/${selectedAgentId.value}/messages`, {
      method: 'POST',
      body: JSON.stringify({ message })
    })
    messageDraft.value = ''
    await refreshDetail()
  } catch (error) {
    showToast(error.message)
  } finally {
    isSending.value = false
  }
}

function handleEnterKey(event) {
  if (!event.shiftKey) {
    event.preventDefault()
    sendMessage()
  }
}

async function cancelAgent(id) {
  try {
    await api(`/agents/${id}/cancel`, { method: 'POST' })
    await refreshDetail()
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
      await api(`/agents/${id}`, { method: 'DELETE' })
      if (selectedAgentId.value === id) {
        selectedAgentId.value = null
        selectedDetail.value = null
      }
      await refreshAgents()
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
    await removeProvider(index)
  }
  confirmDialog.open = true
}

function openProviderDialog(index) {
  providerDialog.index = index
  providerDialog.error = ''
  const provider =
    index === null
      ? {
          id: nextProviderId(),
          name: 'New Provider',
          base_url: 'https://api.openai.com/v1',
          default_model: 'gpt-5.2',
          models: ['gpt-5.2'],
          enabled: true,
          has_api_key: false
        }
      : providersState.providers[index]
  providerDialog.form = {
    id: provider.id || '',
    name: provider.name || '',
    base_url: provider.base_url || '',
    api_key: '',
    default_model: provider.default_model || '',
    modelsText: (provider.models || []).join('\n'),
    enabled: provider.enabled !== false,
    default_provider: providersState.default_provider_id === provider.id,
    has_api_key: provider.has_api_key
  }
  providerDialog.open = true
}

function closeProviderDialog() {
  providerDialog.open = false
}

async function saveProviderDialog() {
  providerDialog.error = ''
  const form = providerDialog.form
  const models = form.modelsText
    .split(/\n|,/)
    .map((value) => value.trim())
    .filter(Boolean)
  if (!form.id || !form.name || !form.base_url || !form.default_model) {
    providerDialog.error = 'Provider ID, display name, base URL, and default model are required.'
    return
  }
  if (!models.includes(form.default_model)) {
    models.unshift(form.default_model)
  }
  const provider = {
    id: form.id,
    name: form.name,
    base_url: form.base_url,
    api_key: form.api_key,
    default_model: form.default_model,
    models,
    enabled: form.enabled
  }
  const nextProviders = [...providersState.providers]
  if (providerDialog.index === null) {
    nextProviders.push(provider)
  } else {
    const existing = nextProviders[providerDialog.index]
    nextProviders[providerDialog.index] = {
      ...existing,
      ...provider,
      has_api_key: Boolean(provider.api_key || existing.has_api_key)
    }
  }
  const defaultProviderId = form.default_provider
    ? provider.id
    : providersState.default_provider_id && nextProviders.some((item) => item.id === providersState.default_provider_id)
      ? providersState.default_provider_id
      : nextProviders[0]?.id || null
  try {
    await saveProviders(nextProviders, defaultProviderId)
    providerDialog.open = false
  } catch (error) {
    providerDialog.error = error.message
  }
}

async function removeProvider(index) {
  const nextProviders = providersState.providers.filter((_, providerIndex) => providerIndex !== index)
  const defaultProviderId = nextProviders.some((provider) => provider.id === providersState.default_provider_id)
    ? providersState.default_provider_id
    : nextProviders[0]?.id || null
  try {
    await saveProviders(nextProviders, defaultProviderId)
  } catch (error) {
    showToast(error.message)
  }
}

async function saveProviders(providers, defaultProviderId) {
  const response = await api('/providers', {
    method: 'PUT',
    body: JSON.stringify({
      providers,
      default_provider_id: defaultProviderId
    })
  })
  providersState.providers = response.providers || []
  providersState.default_provider_id = response.default_provider_id || null
}

function openTokenDialog(title) {
  tokenDialog.title = title
  tokenDialog.value = token
  tokenDialog.open = true
  return new Promise((resolve) => {
    tokenResolver = resolve
  })
}

function saveToken() {
  token = tokenDialog.value.trim()
  localStorage.setItem('maiToken', token)
  tokenDialog.open = false
  if (tokenResolver) tokenResolver()
  tokenResolver = null
  sseRetryCount = 0
  connectEvents()
}

function connectEvents() {
  if (eventSource) {
    eventSource.close()
    eventSource = null
  }
  if (sseRetryTimer) {
    clearTimeout(sseRetryTimer)
    sseRetryTimer = null
  }
  if (!token) {
    connectionState.value = 'offline'
    return
  }
  connectionState.value = 'connecting'
  eventSource = new EventSource(`/events?token=${encodeURIComponent(token)}`)
  eventSource.onopen = () => {
    connectionState.value = 'online'
    sseRetryCount = 0
  }
  eventSource.onerror = () => {
    connectionState.value = 'offline'
    eventSource?.close()
    eventSource = null
    if (sseRetryCount < SSE_MAX_RETRIES) {
      sseRetryCount++
      const delay = Math.min(1000 * Math.pow(2, sseRetryCount - 1), 30000)
      sseRetryTimer = setTimeout(() => connectEvents(), delay)
    } else {
      openTokenDialog('Connection lost. Verify token or check server.')
    }
  }
  const names = [
    'agent_created',
    'agent_status_changed',
    'agent_deleted',
    'turn_started',
    'turn_completed',
    'tool_started',
    'tool_completed',
    'agent_message',
    'error'
  ]
  for (const name of names) {
    eventSource.addEventListener(name, (event) => handleEvent(event))
  }
}

async function handleEvent(event) {
  try {
    const parsed = JSON.parse(event.data)
    eventFeed.value = [parsed, ...eventFeed.value].slice(0, 150)
  } catch {
    eventFeed.value = [
      { sequence: Date.now(), type: 'event', timestamp: new Date().toISOString(), message: event.data },
      ...eventFeed.value
    ].slice(0, 150)
  }
  await refreshAgents()
  if (selectedAgentId.value) await refreshDetail()
}

function emptyProviderForm() {
  return {
    id: '',
    name: '',
    base_url: '',
    api_key: '',
    default_model: '',
    modelsText: '',
    enabled: true,
    default_provider: false,
    has_api_key: false
  }
}

function nextProviderId() {
  let index = providersState.providers.length + 1
  let id = `provider-${index}`
  while (providersState.providers.some((provider) => provider.id === id)) {
    index += 1
    id = `provider-${index}`
  }
  return id
}

function statusTone(status) {
  const value = String(status || '').toLowerCase()
  if (value.includes('run') || value.includes('wait') || value.includes('start')) return 'green'
  if (value.includes('fail') || value.includes('error') || value.includes('cancel')) return 'red'
  if (value.includes('creat') || value.includes('delet')) return 'amber'
  if (value.includes('complet')) return 'blue'
  return 'slate'
}

function formatStatus(status) {
  return String(status || 'unknown')
    .replace(/_/g, ' ')
    .replace(/\b\w/g, (letter) => letter.toUpperCase())
}

function formatDate(value) {
  if (!value) return '-'
  return new Date(value).toLocaleString()
}

function formatTime(value) {
  if (!value) return ''
  return new Date(value).toLocaleTimeString()
}

function formatEventType(type) {
  return String(type || 'event')
    .replace(/_/g, ' ')
    .replace(/\b\w/g, (l) => l.toUpperCase())
}

function totalTokens(agent) {
  return Number(agent.token_usage?.total_tokens || 0).toLocaleString()
}

function shortContainer(value) {
  return value ? value.slice(0, 12) : 'none'
}

function initial(value) {
  return String(value || 'A').charAt(0).toUpperCase()
}

function roleInitial(role) {
  const value = String(role || '').toLowerCase()
  if (value === 'assistant') return 'A'
  if (value === 'system') return 'S'
  if (value === 'tool') return 'T'
  return 'U'
}

function roleLabel(role) {
  const value = String(role || '').toLowerCase()
  if (value === 'assistant') return 'Assistant'
  if (value === 'system') return 'System'
  if (value === 'tool') return 'Tool'
  return 'You'
}

function messageClass(role) {
  return `message-${String(role || 'user').toLowerCase()}`
}

function eventSummary(event) {
  if (event.message) return event.message
  if (event.type === 'agent_created') return `Agent created: ${event.agent?.name || event.agent?.id || ''}`
  if (event.type === 'agent_status_changed') return `${event.agent_id} is ${formatStatus(event.status)}`
  if (event.type === 'agent_deleted') return `Agent deleted: ${event.agent_id}`
  if (event.type === 'turn_started') return `Turn started on ${event.agent_id}`
  if (event.type === 'turn_completed') return `Turn completed with ${formatStatus(event.status)}`
  if (event.type === 'tool_started') return `Tool started: ${event.tool_name}`
  if (event.type === 'tool_completed') return `Tool completed: ${event.tool_name}`
  if (event.type === 'agent_message') {
    const content = event.content || ''
    return `${roleLabel(event.role)}: ${content.length > 120 ? content.slice(0, 120) + '...' : content}`
  }
  return JSON.stringify(event)
}

function showToast(message) {
  toast.value = message
  window.clearTimeout(showToast.timer)
  showToast.timer = window.setTimeout(() => {
    toast.value = ''
  }, 4000)
}
</script>
