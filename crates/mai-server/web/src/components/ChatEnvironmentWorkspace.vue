<template>
  <section class="chat-workspace">
    <div v-if="!detail" class="empty-stage">
      <div class="empty-mark">C</div>
      <h2>No environment selected</h2>
      <button class="primary-button" type="button" @click="$emit('create-environment')">Create Environment</button>
    </div>

    <template v-else>
      <header class="chat-workspace-head">
        <div class="chat-title-stack">
          <div ref="conversationMenuRef" class="workspace-menu-wrap">
            <button
              class="workspace-title-trigger"
              type="button"
              :aria-expanded="conversationMenuOpen ? 'true' : 'false'"
              @click="toggleConversationMenu"
            >
              <span class="workspace-title-main">{{ currentConversationTitle }}</span>
              <span class="workspace-chevron">⌄</span>
            </button>
            <div v-if="conversationMenuOpen" class="workspace-menu conversation-menu">
              <button
                v-for="conversation in conversations"
                :key="conversation.id"
                class="workspace-menu-option"
                :class="{ active: conversation.id === selectedConversationId }"
                type="button"
                @click="selectConversation(conversation.id)"
              >
                <span>
                  <strong>{{ conversationTitle(conversation) }}</strong>
                  <small>{{ conversationMeta(conversation) }}</small>
                </span>
                <span v-if="conversation.id === selectedConversationId">✓</span>
              </button>
              <div v-if="!conversations.length" class="workspace-menu-empty">No conversations</div>
            </div>
          </div>

          <div ref="environmentMenuRef" class="workspace-menu-wrap">
            <button
              class="workspace-subtitle-trigger"
              type="button"
              :aria-expanded="environmentMenuOpen ? 'true' : 'false'"
              @click="toggleEnvironmentMenu"
            >
              <span>{{ detail.name || 'Environment' }}</span>
              <span class="workspace-meta-dot">·</span>
              <span>{{ environmentStatus }}</span>
              <span class="workspace-chevron">⌄</span>
            </button>
            <div v-if="environmentMenuOpen" class="workspace-menu environment-menu">
              <button
                v-for="environment in environments"
                :key="environment.id"
                class="workspace-menu-option"
                :class="{ active: environment.id === selectedEnvironmentId }"
                type="button"
                @click="selectEnvironment(environment.id)"
              >
                <span>
                  <strong>{{ environment.name || environment.id }}</strong>
                  <small>{{ environmentMeta(environment) }}</small>
                </span>
                <span
                  v-if="environment.id === selectedEnvironmentId"
                  class="workspace-check"
                >✓</span>
                <span v-else class="status-dot" :class="statusTone(environment.status)" />
              </button>
              <div v-if="!environments.length" class="workspace-menu-empty">No environments</div>
              <button class="workspace-menu-create" type="button" @click="createEnvironment">
                <span>+</span>
                <strong>Create Environment</strong>
              </button>
            </div>
          </div>
        </div>

        <div class="chat-head-actions">
          <AgentModelPicker
            v-if="agent"
            :detail="agent"
            :providers="providers"
            :reasoning-effort="currentReasoningEffort"
            :disabled="modelBusy"
            :updating="updatingModel"
            @apply="$emit('update-model', $event)"
            @open-providers="$emit('open-providers')"
          />
          <button
            class="primary-button"
            type="button"
            :disabled="!detail || sending"
            @click="$emit('create-conversation')"
          >
            New Conversation
          </button>
        </div>
      </header>

      <div class="chat-body">
        <ChatTimeline
          v-if="agent"
          v-model:conversation-ref="conversationRef"
          :timeline-items="timelineItems"
          :status-item="containerStatusItem"
          :loading="loading"
          :is-tool-expanded="isToolExpanded"
          :trace-state="traceState"
          @toggle-tool="toggleTool"
        />
      </div>

      <ContextStatusLine
        v-if="agent"
        :detail="agent"
        :context-capacity="contextCapacity"
        :activity="latestActivity"
      />

      <PlanApprovalBar
        v-if="planApprovalPending"
        :plan-version="planVersion"
        :approving="false"
        :submitting-revision="false"
        @approve="$emit('send', approvalMessage)"
        @request-revision="(feedback) => $emit('send', feedback)"
      />
      <QuestionBar
        v-else-if="pendingUserInput"
        :questions="pendingUserInput.questions"
        :header="pendingUserInput.header"
        :sending="sending"
        @answer="handleQuestionAnswer"
      />
      <ComposerBar
        v-else
        :draft="draft"
        :sending="sending"
        :skills="skills"
        :selected-skills="selectedSkills"
        :skills-loading="skillsLoading"
        :skills-error="skillsError"
        :stoppable="canStopTurn"
        :stopping="stopping"
        :disabled="composerDisabled"
        @send="$emit('send', $event)"
        @stop="$emit('stop', agent)"
        @update:draft="$emit('update:draft', $event)"
        @update:selected-skills="$emit('update:selectedSkills', $event)"
        @load-skills="$emit('load-skills')"
      />
    </template>
  </section>
</template>

<script setup>
import { computed, nextTick, onBeforeUnmount, reactive, ref, watch } from 'vue'
import AgentModelPicker from './AgentModelPicker.vue'
import ChatTimeline from './ChatTimeline.vue'
import ComposerBar from './ComposerBar.vue'
import ContextStatusLine from './ContextStatusLine.vue'
import PlanApprovalBar from './PlanApprovalBar.vue'
import QuestionBar from './QuestionBar.vue'
import { useApi } from '../composables/useApi'
import { formatDate, formatStatus, statusTone } from '../utils/format'
import { buildAgentTimeline } from '../utils/timeline'
import { defaultReasoningEffort, reasoningOptionsFor } from '../utils/reasoning'

const props = defineProps({
  environments: { type: Array, required: true },
  detail: { type: Object, default: null },
  selectedEnvironmentId: { type: String, default: null },
  selectedConversationId: { type: String, default: null },
  events: { type: Array, required: true },
  draft: { type: String, default: '' },
  loading: { type: Boolean, default: false },
  sending: { type: Boolean, default: false },
  stopping: { type: Boolean, default: false },
  updatingModel: { type: Boolean, default: false },
  providers: { type: Array, default: () => [] },
  skills: { type: Array, default: () => [] },
  selectedSkills: { type: Array, default: () => [] },
  skillsLoading: { type: Boolean, default: false },
  skillsError: { type: String, default: '' }
})

const conversationRef = defineModel('conversationRef', { default: null })

const emit = defineEmits([
  'create-environment',
  'select-environment',
  'select-conversation',
  'create-conversation',
  'send',
  'stop',
  'update:draft',
  'update:selectedSkills',
  'load-skills',
  'update-model',
  'open-providers'
])

const { api, showToast } = useApi()
const conversationMenuRef = ref(null)
const environmentMenuRef = ref(null)
const conversationMenuOpen = ref(false)
const environmentMenuOpen = ref(false)
const expandedTools = reactive({})
const traces = reactive({})
const answeredInputKeys = reactive(new Set())
const emptyTrace = { loading: false, error: '', detail: null }
const currentReasoningEffort = ref('')
const approvalMessage = 'Approve the current plan and proceed.'

const agent = computed(() => props.detail?.root_agent || null)
const conversations = computed(() => props.detail?.conversations || agent.value?.sessions || [])
const currentConversation = computed(() => {
  return conversations.value.find((conversation) => conversation.id === props.selectedConversationId)
    || conversations.value.find((conversation) => conversation.id === props.detail?.selected_conversation_id)
    || null
})
const currentConversationTitle = computed(() => {
  if (currentConversation.value) return conversationTitle(currentConversation.value)
  if (props.selectedConversationId) return `Conversation ${props.selectedConversationId.slice(0, 8)}`
  return 'New Conversation'
})
const environmentStatus = computed(() => formatStatus(props.detail?.status))
const timelineItems = computed(() => buildAgentTimeline(agent.value, props.events))
const currentProvider = computed(() => props.providers.find((provider) => provider.id === agent.value?.provider_id))
const currentModel = computed(() => currentProvider.value?.models?.find((model) => model.id === agent.value?.model))
const currentReasoningOptions = computed(() => reasoningOptionsFor(currentProvider.value, currentModel.value))
const modelBusy = computed(() => props.updatingModel || ['running_turn', 'waiting_tool', 'starting_container'].includes(agent.value?.status))
const containerStarting = computed(() => agent.value?.status === 'starting_container' || agent.value?.status === 'created')
const composerDisabled = computed(() => props.sending || containerStarting.value)
const containerStatusItem = computed(() => {
  if (!agent.value) return null
  if (agent.value.status === 'starting_container') {
    return {
      type: 'process',
      key: `container-starting-${agent.value.id}`,
      tone: 'active',
      label: 'Starting Container',
      detail: agent.value.docker_image ? `Preparing ${agent.value.docker_image}` : 'Preparing environment container'
    }
  }
  if (agent.value.status === 'created') {
    return {
      type: 'process',
      key: `container-created-${agent.value.id}`,
      tone: 'muted',
      label: 'Preparing Environment',
      detail: 'Container startup will begin shortly'
    }
  }
  if (agent.value.status === 'failed') {
    return {
      type: 'process',
      key: `container-failed-${agent.value.id}`,
      tone: 'error',
      label: 'Container Failed',
      detail: agent.value.last_error || 'Environment container failed to start'
    }
  }
  return null
})
const canStopTurn = computed(() => {
  if (!agent.value?.current_turn) return false
  return ['running_turn', 'waiting_tool', 'starting_container'].includes(agent.value.status)
})
const pendingUserInput = computed(() => {
  const input = timelineItems.value.find((item) => item.type === 'user_input' && !answeredInputKeys.has(item.key))
  if (!input) return null
  const inputIdx = timelineItems.value.indexOf(input)
  const answered = timelineItems.value.some(
    (item, idx) => idx > inputIdx && item.type === 'message' && item.role === 'user'
  )
  return answered ? null : input
})
const planApprovalPending = computed(() => Boolean(props.detail?.plan_approval_pending || props.detail?.plan?.status === 'ready'))
const planVersion = computed(() => Number(props.detail?.plan?.version || props.detail?.plan_version || 0))
const latestActivity = computed(() => {
  if (agent.value?.status === 'starting_container') {
    return agent.value?.docker_image ? `Preparing ${agent.value.docker_image}` : 'Preparing environment container'
  }
  const activeTool = [...timelineItems.value].reverse().find((item) => isToolTimelineItem(item) && item.status === 'running')
  if (activeTool) return `${activeTool.toolActionLabel || 'Running'} ${activeTool.toolName}`
  const activeProcess = [...timelineItems.value].reverse().find((item) => item.type === 'process' && item.tone === 'active')
  if (activeProcess) return [activeProcess.label, activeProcess.detail].filter(Boolean).join(' · ')
  return ''
})
const contextCapacity = computed(() => {
  const usage = agent.value?.context_usage
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

watch(
  () => [agent.value?.id, agent.value?.selected_session_id],
  () => {
    for (const key of Object.keys(expandedTools)) delete expandedTools[key]
    for (const key of Object.keys(traces)) delete traces[key]
    answeredInputKeys.clear()
    syncCurrentReasoningEffort()
  },
  { immediate: true }
)

watch(
  () => [agent.value?.reasoning_effort, currentReasoningOptions.value.map((option) => option.value).join('|')],
  syncCurrentReasoningEffort,
  { immediate: true }
)

function toggleConversationMenu() {
  conversationMenuOpen.value = !conversationMenuOpen.value
  if (conversationMenuOpen.value) environmentMenuOpen.value = false
  toggleDocumentListener()
}

function toggleEnvironmentMenu() {
  environmentMenuOpen.value = !environmentMenuOpen.value
  if (environmentMenuOpen.value) conversationMenuOpen.value = false
  toggleDocumentListener()
}

function selectConversation(id) {
  conversationMenuOpen.value = false
  toggleDocumentListener()
  emit('select-conversation', id)
}

function selectEnvironment(id) {
  environmentMenuOpen.value = false
  toggleDocumentListener()
  emit('select-environment', id)
}

function createEnvironment() {
  environmentMenuOpen.value = false
  toggleDocumentListener()
  emit('create-environment')
}

function conversationTitle(conversation) {
  return conversation?.title || conversation?.name || `Conversation ${String(conversation?.id || '').slice(0, 8)}`
}

function conversationMeta(conversation) {
  const count = conversation?.message_count ?? conversation?.turn_count ?? null
  const updated = conversation?.updated_at ? formatDate(conversation.updated_at) : ''
  return [
    count === null ? '' : `${count} message${count === 1 ? '' : 's'}`,
    updated
  ].filter(Boolean).join(' · ') || conversation?.id || ''
}

function environmentMeta(environment) {
  const conversationsCount = environment.conversation_count ?? 0
  return [
    formatStatus(environment.status),
    `${conversationsCount} conversation${conversationsCount === 1 ? '' : 's'}`,
    environment.docker_image || ''
  ].filter(Boolean).join(' · ')
}

function syncCurrentReasoningEffort() {
  const activeValue = agent.value?.reasoning_effort || ''
  currentReasoningEffort.value = currentReasoningOptions.value.some((option) => option.value === activeValue)
    ? activeValue
    : defaultReasoningEffort(currentProvider.value, currentModel.value)
}

function handleQuestionAnswer(responseText) {
  if (!pendingUserInput.value) return
  answeredInputKeys.add(pendingUserInput.value.key)
  emit('send', responseText)
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
      `/agents/${agent.value.id}/sessions/${agent.value.selected_session_id}/tool-calls/${encodeURIComponent(item.callId)}`
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

function isToolTimelineItem(item) {
  return item?.type === 'tool_call' || item?.type === 'tool'
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

function toggleDocumentListener() {
  document.removeEventListener('pointerdown', onDocumentPointerDown)
  if (conversationMenuOpen.value || environmentMenuOpen.value) {
    document.addEventListener('pointerdown', onDocumentPointerDown)
  }
}

function onDocumentPointerDown(event) {
  if (conversationMenuRef.value?.contains(event.target) || environmentMenuRef.value?.contains(event.target)) return
  conversationMenuOpen.value = false
  environmentMenuOpen.value = false
  toggleDocumentListener()
}

onBeforeUnmount(() => {
  document.removeEventListener('pointerdown', onDocumentPointerDown)
})
</script>
