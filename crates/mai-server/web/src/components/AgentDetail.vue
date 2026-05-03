<template>
  <template v-if="detail">
    <div class="agent-header">
      <div class="large-avatar">{{ initial(detail.name) }}</div>
      <div class="agent-title">
        <div class="title-row">
          <h2>{{ detail.name }}</h2>
          <span class="status-pill" :class="statusTone(detail.status)">
            {{ formatStatus(detail.status) }}
          </span>
        </div>
        <p>{{ detail.provider_name }} / {{ detail.model }}</p>
        <p v-if="detail.last_error" class="error-text">{{ detail.last_error }}</p>
      </div>
      <div class="thinking-depth-control" :class="{ disabled: !currentReasoningOptions.length }">
        <label for="thinking-depth-select">
          <span>思考深度</span>
          <select
            id="thinking-depth-select"
            v-model="currentReasoningEffort"
            :disabled="!currentReasoningOptions.length || isModelChangeBusy || updatingModel"
            @change="saveReasoningEffort"
          >
            <option v-if="!currentReasoningOptions.length" value="">
              当前模型不支持
            </option>
            <option v-for="option in currentReasoningOptions" :key="option.value" :value="option.value">
              {{ option.label }}
            </option>
          </select>
        </label>
        <small v-if="currentReasoningOptions.length">
          {{ updatingModel ? '保存中...' : '应用到下一轮对话' }}
        </small>
        <small v-else>换到支持思考的模型后可选</small>
      </div>
      <div class="agent-actions">
        <button class="ghost-button" :disabled="!providers.length || isModelChangeBusy || updatingModel" @click="openModelEditor">
          Change Model
        </button>
        <button class="ghost-button" @click="$emit('cancel', detail.id)">Cancel</button>
        <button class="danger-button" @click="$emit('delete', detail.id, detail.name)">Delete</button>
      </div>
    </div>

    <div v-if="modelEditor.open" class="agent-model-editor">
      <label>
        <span>Provider</span>
        <select v-model="modelEditor.provider_id" @change="onModelProviderChanged">
          <option v-for="provider in providers" :key="provider.id" :value="provider.id">
            {{ provider.name }}
          </option>
        </select>
      </label>
      <label>
        <span>Model</span>
        <select v-model="modelEditor.model" @change="onModelChanged">
          <option v-for="model in editorModels" :key="model.id" :value="model.id">
            {{ model.name || model.id }}
          </option>
        </select>
      </label>
      <label v-if="editorReasoningOptions.length">
        <span>思考深度</span>
        <select v-model="modelEditor.reasoning_effort">
          <option v-for="option in editorReasoningOptions" :key="option.value" :value="option.value">
            {{ option.label }}
          </option>
        </select>
      </label>
      <div class="agent-model-actions">
        <button class="ghost-button" type="button" @click="modelEditor.open = false">Cancel</button>
        <button class="primary-button" type="button" :disabled="!modelEditor.model || updatingModel" @click="saveModelEdit">
          Save
        </button>
      </div>
      <p v-if="modelEditor.error" class="dialog-error">{{ modelEditor.error }}</p>
    </div>

    <div class="detail-strip">
      <div><span>Status</span><strong>{{ formatStatus(detail.status) }}</strong></div>
      <div><span>Container</span><strong>{{ shortContainer(detail.container_id) }}</strong></div>
      <div><span>Provider</span><strong>{{ detail.provider_id }}</strong></div>
      <div v-if="detail.reasoning_effort"><span>Reasoning</span><strong>{{ reasoningLabel(detail.reasoning_effort) }}</strong></div>
      <div><span>Tokens</span><strong>{{ totalTokens(detail) }}</strong></div>
      <div><span>Created</span><strong>{{ formatDate(detail.created_at) }}</strong></div>
    </div>

    <div class="session-strip">
      <div class="session-tabs">
        <button
          v-for="session in detail.sessions || []"
          :key="session.id"
          type="button"
          class="session-tab"
          :class="{ active: session.id === detail.selected_session_id }"
          @click="$emit('select-session', session.id)"
        >
          <span>{{ session.title }}</span>
          <small>{{ session.message_count }}</small>
        </button>
      </div>
      <button class="small-button" type="button" @click="$emit('create-session')">New Chat</button>
    </div>

    <div class="agent-body">
      <section class="conversation chat-timeline" ref="conversationRef">
        <div v-if="loading" class="loading-center">
          <div class="spinner"></div>
          <span>Loading messages...</span>
        </div>
        <div v-else-if="!timelineItems.length" class="quiet-empty">
          <strong>No messages yet</strong>
          <span>Send the first instruction to start a turn.</span>
        </div>
        <template v-else>
          <article
            v-for="item in timelineItems"
            :key="item.key"
            class="timeline-item"
            :class="`timeline-${item.type}`"
          >
            <template v-if="item.type === 'message'">
              <div class="timeline-message" :class="messageClass(item.role)">
                <div class="message-avatar">{{ roleInitial(item.role) }}</div>
                <div class="message-content">
                  <span>{{ roleLabel(item.role) }}</span>
                  <div class="markdown-body" v-html="renderMarkdown(item.content)"></div>
                </div>
              </div>
            </template>

            <div v-else-if="item.type === 'tool'" class="tool-card" :class="`tool-${item.status}`">
              <button class="tool-card-toggle" type="button" @click="toggleTool(item)">
                <span class="tool-chevron" :class="{ open: isToolExpanded(item) }">›</span>
                <span class="tool-state-dot"></span>
                <span class="tool-title">
                  <span>{{ item.status === 'running' ? 'Calling tool' : 'Used tool' }}</span>
                  <strong>{{ item.toolName }}</strong>
                </span>
                <span class="tool-meta">
                  {{ toolStatusLabel(item.status) }}
                  <template v-if="formatDuration(item.durationMs)"> · {{ formatDuration(item.durationMs) }}</template>
                </span>
              </button>
              <div class="tool-preview-grid">
                <div v-if="item.argumentsPreview" class="trace-preview">
                  <span>Arguments</span>
                  <div class="trace-surface" v-html="renderToolTrace({ toolName: item.toolName, kind: 'arguments', value: item.argumentsPreview })"></div>
                </div>
                <div v-if="item.outputPreview" class="trace-preview">
                  <span>Output</span>
                  <div class="trace-surface" v-html="renderToolTrace({ toolName: item.toolName, kind: 'output', value: item.outputPreview })"></div>
                </div>
              </div>
              <div v-if="isToolExpanded(item)" class="tool-trace">
                <div v-if="traceState(item).loading" class="trace-loading">
                  <span class="spinner-sm"></span>
                  Loading full trace...
                </div>
                <p v-else-if="traceState(item).error" class="trace-error">
                  {{ traceState(item).error }}
                </p>
                <template v-else-if="traceState(item).detail">
                  <div class="trace-block">
                    <span>Full arguments</span>
                    <div class="trace-surface" v-html="renderToolTrace({ toolName: item.toolName, kind: 'arguments', value: traceState(item).detail.arguments })"></div>
                  </div>
                  <div class="trace-block">
                    <span>Full output</span>
                    <div class="trace-surface" v-html="renderToolTrace({ toolName: item.toolName, kind: 'output', value: traceState(item).detail.output })"></div>
                  </div>
                </template>
              </div>
            </div>

            <div v-else-if="item.type === 'error'" class="timeline-error-card">
              <strong>Error</strong>
              <p>{{ item.message }}</p>
            </div>

            <div v-else class="process-row" :class="`process-${item.tone}`">
              <span class="process-dot"></span>
              <span class="process-label">{{ item.label }}</span>
              <span v-if="item.detail" class="process-detail">{{ item.detail }}</span>
            </div>
          </article>
        </template>
      </section>
    </div>

    <form class="composer" @submit.prevent="send">
      <textarea
        :value="draft"
        rows="2"
        placeholder="Send a command or message... (Enter to send, Shift+Enter for new line)"
        @input="$emit('update:draft', $event.target.value)"
        @keydown.enter.exact="handleEnter"
      />
      <button class="primary-button" type="submit" :disabled="!draft.trim() || sending">
        <span v-if="sending" class="spinner-sm"></span>
        <template v-else>Send</template>
      </button>
    </form>
  </template>
</template>

<script setup>
import { computed, nextTick, reactive, ref, watch } from 'vue'
import {
  formatStatus, formatDate,
  totalTokens, shortContainer, initial, roleInitial, roleLabel,
  statusTone, messageClass
} from '../utils/format'
import { defaultReasoningEffort, reasoningLabel, reasoningOptionsFor } from '../utils/reasoning'
import { renderMarkdown } from '../utils/markdown'
import { useApi } from '../composables/useApi'
import {
  buildAgentTimeline,
  formatDuration,
  renderToolTrace,
  toolStatusLabel
} from '../utils/timeline'

const props = defineProps({
  detail: { type: Object, default: null },
  events: { type: Array, required: true },
  draft: { type: String, default: '' },
  loading: { type: Boolean, default: false },
  sending: { type: Boolean, default: false },
  updatingModel: { type: Boolean, default: false },
  providers: { type: Array, default: () => [] }
})

const conversationRef = defineModel('conversationRef', { default: null })
const emit = defineEmits([
  'cancel',
  'delete',
  'send',
  'update:draft',
  'update-model',
  'create-session',
  'select-session'
])
const { api, showToast } = useApi()
const expandedTools = reactive({})
const traces = reactive({})
const emptyTrace = { loading: false, error: '', detail: null }
const currentReasoningEffort = ref('')
const modelEditor = reactive({
  open: false,
  provider_id: '',
  model: '',
  reasoning_effort: '',
  error: ''
})

const timelineItems = computed(() => buildAgentTimeline(props.detail, props.events))
const editorProvider = computed(() => props.providers.find((provider) => provider.id === modelEditor.provider_id))
const editorModels = computed(() => editorProvider.value?.models || [])
const editorModel = computed(() => editorModels.value.find((model) => model.id === modelEditor.model))
const editorReasoningOptions = computed(() => reasoningOptionsFor(editorProvider.value, editorModel.value))
const currentProvider = computed(() => props.providers.find((provider) => provider.id === props.detail?.provider_id))
const currentModel = computed(() => currentProvider.value?.models?.find((model) => model.id === props.detail?.model))
const currentReasoningOptions = computed(() => reasoningOptionsFor(currentProvider.value, currentModel.value))
const isModelChangeBusy = computed(() => {
  const status = props.detail?.status
  return status === 'running_turn' || status === 'waiting_tool' || status === 'starting_container'
})

watch(
  () => props.detail?.id,
  () => {
    for (const key of Object.keys(expandedTools)) delete expandedTools[key]
    for (const key of Object.keys(traces)) delete traces[key]
    modelEditor.open = false
    syncCurrentReasoningEffort()
  }
)

watch(
  () => [props.detail?.reasoning_effort, currentReasoningOptions.value.map((option) => option.value).join('|')],
  syncCurrentReasoningEffort,
  { immediate: true }
)

watch(
  () => props.detail?.selected_session_id,
  () => {
    for (const key of Object.keys(expandedTools)) delete expandedTools[key]
    for (const key of Object.keys(traces)) delete traces[key]
  }
)

function handleEnter(event) {
  if (!event.shiftKey) {
    event.preventDefault()
    send()
  }
}

function send() {
  if (props.draft.trim()) emit('send', props.draft.trim())
}

function syncCurrentReasoningEffort() {
  const activeValue = props.detail?.reasoning_effort || ''
  currentReasoningEffort.value = currentReasoningOptions.value.some((option) => option.value === activeValue)
    ? activeValue
    : defaultReasoningEffort(currentProvider.value, currentModel.value)
}

function saveReasoningEffort() {
  if (!props.detail || !currentReasoningOptions.value.length) return
  emit('update-model', {
    provider_id: props.detail.provider_id,
    model: props.detail.model,
    reasoning_effort: currentReasoningEffort.value
  })
}

function openModelEditor() {
  modelEditor.open = true
  modelEditor.provider_id = props.detail?.provider_id || props.providers[0]?.id || ''
  modelEditor.model = props.detail?.model || editorProvider.value?.default_model || editorModels.value[0]?.id || ''
  modelEditor.reasoning_effort = props.detail?.reasoning_effort || defaultReasoningEffort(editorProvider.value, editorModel.value)
  modelEditor.error = ''
}

function onModelProviderChanged() {
  modelEditor.model = editorProvider.value?.default_model || editorModels.value[0]?.id || ''
  resetModelEditorReasoningEffort()
}

function onModelChanged() {
  resetModelEditorReasoningEffort()
}

function resetModelEditorReasoningEffort() {
  modelEditor.reasoning_effort = defaultReasoningEffort(editorProvider.value, editorModel.value)
}

function saveModelEdit() {
  if (!modelEditor.provider_id || !modelEditor.model) {
    modelEditor.error = 'Provider and model are required.'
    return
  }
  emit('update-model', {
    provider_id: modelEditor.provider_id,
    model: modelEditor.model,
    reasoning_effort: modelEditor.reasoning_effort
  })
  modelEditor.open = false
}

function isToolExpanded(item) {
  return Boolean(expandedTools[item.callId])
}

function traceState(item) {
  return traces[item.callId] || emptyTrace
}

function ensureTraceState(item) {
  if (!traces[item.callId]) traces[item.callId] = { loading: false, error: '', detail: null }
  return traces[item.callId]
}

async function toggleTool(item) {
  const shouldStayPinned = isNearTimelineBottom()
  expandedTools[item.callId] = !expandedTools[item.callId]
  if (!expandedTools[item.callId]) return

  const state = ensureTraceState(item)
  if (state.detail || state.loading) {
    if (shouldStayPinned) await scrollTimelineToBottom()
    return
  }
  state.loading = true
  state.error = ''
  try {
    state.detail = await api(
      `/agents/${props.detail.id}/sessions/${props.detail.selected_session_id}/tool-calls/${encodeURIComponent(item.callId)}`
    )
  } catch (error) {
    state.error = error.message
    showToast(error.message)
  } finally {
    state.loading = false
    if (shouldStayPinned) await scrollTimelineToBottom()
  }
}

function isNearTimelineBottom() {
  const element = conversationRef.value
  if (!element) return true
  return element.scrollHeight - element.scrollTop - element.clientHeight < 120
}

async function scrollTimelineToBottom() {
  await nextTick()
  await new Promise((resolve) => requestAnimationFrame(() => resolve()))
  if (conversationRef.value) {
    conversationRef.value.scrollTop = conversationRef.value.scrollHeight
  }
}
</script>
