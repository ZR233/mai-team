<template>
  <template v-if="detail">
    <AgentHeader
      :detail="detail"
      :reasoning-effort="currentReasoningEffort"
      :reasoning-options="currentReasoningOptions"
      :providers-count="providers.length"
      :model-change-busy="isModelChangeBusy"
      :updating-model="updatingModel"
      @change-reasoning-effort="saveReasoningEffort"
      @open-model="openModelEditor"
      @cancel="$emit('cancel', $event)"
      @delete="(...args) => $emit('delete', ...args)"
    />

    <div v-if="modelEditor.open" class="agent-model-editor">
      <ModelSelector
        v-model:provider-id="modelEditor.provider_id"
        v-model:model="modelEditor.model"
        v-model:reasoning-effort="modelEditor.reasoning_effort"
        :providers="providers"
        compact
      />
      <div class="agent-model-actions">
        <button class="ghost-button" type="button" @click="modelEditor.open = false">Cancel</button>
        <button class="primary-button" type="button" :disabled="!modelEditor.model || updatingModel" @click="saveModelEdit">
          Save
        </button>
      </div>
      <p v-if="modelEditor.error" class="dialog-error">{{ modelEditor.error }}</p>
    </div>

    <SessionTabs
      v-if="showSessions"
      :sessions="detail.sessions || []"
      :selected-session-id="detail.selected_session_id"
      @select-session="$emit('select-session', $event)"
      @create-session="$emit('create-session')"
    />

    <div class="agent-body">
      <ChatTimeline
        v-model:conversation-ref="conversationRef"
        :timeline-items="timelineItems"
        :loading="loading"
        :is-tool-expanded="isToolExpanded"
        :trace-state="traceState"
        @toggle-tool="toggleTool"
      />
    </div>

    <ContextStatusLine
      :detail="detail"
      :context-capacity="contextCapacity"
      :activity="latestActivity"
    />

    <PlanApprovalBar
      v-if="planApprovalPending"
      :plan-version="planVersion"
      :approving="approvingPlan"
      :submitting-revision="false"
      @approve="$emit('approve-plan')"
      @request-revision="$emit('request-plan-revision', $event)"
    />
    <QuestionBar
      v-else-if="inputEnabled && pendingUserInput"
      :questions="pendingUserInput.questions"
      :header="pendingUserInput.header"
      :sending="sending"
      @answer="handleQuestionAnswer"
    />
    <ComposerBar
      v-else-if="showComposer"
      :draft="draft"
      :sending="sending"
      :skills="skills"
      :selected-skills="selectedSkills"
      :skills-loading="skillsLoading"
      :skills-error="skillsError"
      @send="$emit('send', $event)"
      @update:draft="$emit('update:draft', $event)"
      @update:selected-skills="$emit('update:selectedSkills', $event)"
      @load-skills="$emit('load-skills')"
    />
  </template>
</template>

<script setup>
import { computed, nextTick, reactive, ref, watch } from 'vue'
import AgentHeader from './AgentHeader.vue'
import ChatTimeline from './ChatTimeline.vue'
import ComposerBar from './ComposerBar.vue'
import QuestionBar from './QuestionBar.vue'
import PlanApprovalBar from './PlanApprovalBar.vue'
import ContextStatusLine from './ContextStatusLine.vue'
import ModelSelector from './ModelSelector.vue'
import SessionTabs from './SessionTabs.vue'
import { defaultReasoningEffort, reasoningOptionsFor } from '../utils/reasoning'
import { useApi } from '../composables/useApi'
import {
  buildAgentTimeline
} from '../utils/timeline'

const props = defineProps({
  detail: { type: Object, default: null },
  events: { type: Array, required: true },
  draft: { type: String, default: '' },
  loading: { type: Boolean, default: false },
  sending: { type: Boolean, default: false },
  updatingModel: { type: Boolean, default: false },
  providers: { type: Array, default: () => [] },
  skills: { type: Array, default: () => [] },
  selectedSkills: { type: Array, default: () => [] },
  skillsLoading: { type: Boolean, default: false },
  skillsError: { type: String, default: '' },
  showSessions: { type: Boolean, default: true },
  showComposer: { type: Boolean, default: true },
  planApprovalPending: { type: Boolean, default: false },
  approvingPlan: { type: Boolean, default: false },
  planVersion: { type: Number, default: 0 },
  inputEnabled: { type: Boolean, default: true }
})

const conversationRef = defineModel('conversationRef', { default: null })
const emit = defineEmits([
  'cancel',
  'delete',
  'send',
  'update:draft',
  'update:selectedSkills',
  'load-skills',
  'update-model',
  'create-session',
  'select-session',
  'approve-plan',
  'request-plan-revision'
])
const { api, showToast } = useApi()
const expandedTools = reactive({})
const traces = reactive({})
const answeredInputKeys = reactive(new Set())
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
const pendingUserInput = computed(() => {
  if (!props.inputEnabled) return null
  const items = timelineItems.value
  const input = items.find(
    (item) => item.type === 'user_input' && !answeredInputKeys.has(item.key)
  )
  if (!input) return null
  // If there's a user message after this input, the question was already answered
  const inputIdx = items.indexOf(input)
  const answered = items.some(
    (item, idx) => idx > inputIdx && item.type === 'message' && item.role === 'user'
  )
  return answered ? null : input
})
const editorProvider = computed(() => props.providers.find((provider) => provider.id === modelEditor.provider_id))
const editorModels = computed(() => editorProvider.value?.models || [])
const editorModel = computed(() => editorModels.value.find((model) => model.id === modelEditor.model))
const currentProvider = computed(() => props.providers.find((provider) => provider.id === props.detail?.provider_id))
const currentModel = computed(() => currentProvider.value?.models?.find((model) => model.id === props.detail?.model))
const currentReasoningOptions = computed(() => reasoningOptionsFor(currentProvider.value, currentModel.value))
const latestActivity = computed(() => {
  const activeTool = [...timelineItems.value].reverse().find((item) => item.type === 'tool' && item.status === 'running')
  if (activeTool) return `${activeTool.toolActionLabel || 'Running'} ${activeTool.toolName}`
  const activeProcess = [...timelineItems.value].reverse().find((item) => item.type === 'process' && item.tone === 'active')
  if (activeProcess) return [activeProcess.label, activeProcess.detail].filter(Boolean).join(' · ')
  return ''
})
const contextCapacity = computed(() => {
  const usage = props.detail?.context_usage
  const total = Number(usage?.context_tokens || currentModel.value?.context_tokens || 0)
  if (!total) {
    return {
      visible: false,
      percent: 0,
      threshold: 80,
      used: '0',
      total: '0',
      label: '0%',
      tone: 'low'
    }
  }
  const rawUsed = Number(usage?.used_tokens || 0)
  const rawThreshold = Number(usage?.threshold_percent || 80)
  const percentValue = Math.max(0, Math.min(100, Math.round((rawUsed / total) * 100)))
  const threshold = Math.max(1, Math.min(100, Math.round(rawThreshold)))
  return {
    visible: true,
    percent: percentValue,
    threshold,
    used: formatCompactNumber(rawUsed),
    total: formatCompactNumber(total),
    label: `${percentValue}%`,
    tone: percentValue >= threshold ? 'full' : percentValue >= Math.max(1, threshold - 15) ? 'warm' : 'low'
  }
})
const isModelChangeBusy = computed(() => {
  const status = props.detail?.status
  return status === 'running_turn' || status === 'waiting_tool' || status === 'starting_container'
})

watch(
  () => props.detail?.id,
  () => {
    for (const key of Object.keys(expandedTools)) delete expandedTools[key]
    for (const key of Object.keys(traces)) delete traces[key]
    answeredInputKeys.clear()
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
    answeredInputKeys.clear()
  }
)

function syncCurrentReasoningEffort() {
  const activeValue = props.detail?.reasoning_effort || ''
  currentReasoningEffort.value = currentReasoningOptions.value.some((option) => option.value === activeValue)
    ? activeValue
    : defaultReasoningEffort(currentProvider.value, currentModel.value)
}

function handleQuestionAnswer(responseText) {
  if (!pendingUserInput.value) return
  answeredInputKeys.add(pendingUserInput.value.key)
  emit('send', responseText)
}

function saveReasoningEffort(value = currentReasoningEffort.value) {
  if (!props.detail || !currentReasoningOptions.value.length) return
  currentReasoningEffort.value = value
  emit('update-model', {
    provider_id: props.detail.provider_id,
    model: props.detail.model,
    reasoning_effort: value
  })
}

function openModelEditor() {
  modelEditor.open = true
  modelEditor.provider_id = props.detail?.provider_id || props.providers[0]?.id || ''
  modelEditor.model = props.detail?.model || editorProvider.value?.default_model || editorModels.value[0]?.id || ''
  modelEditor.reasoning_effort = props.detail?.reasoning_effort || defaultReasoningEffort(editorProvider.value, editorModel.value)
  modelEditor.error = ''
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

function formatCompactNumber(value) {
  const number = Number(value || 0)
  if (number >= 1_000_000) return `${trimNumber(number / 1_000_000)}M`
  if (number >= 1_000) return `${trimNumber(number / 1_000)}K`
  return String(Math.round(number))
}

function trimNumber(value) {
  return value >= 10 ? String(Math.round(value)) : value.toFixed(1).replace(/\.0$/, '')
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
