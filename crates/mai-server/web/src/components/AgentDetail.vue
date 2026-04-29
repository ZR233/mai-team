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
      <div class="agent-actions">
        <button class="ghost-button" @click="$emit('cancel', detail.id)">Cancel</button>
        <button class="danger-button" @click="$emit('delete', detail.id, detail.name)">Delete</button>
      </div>
    </div>

    <div class="detail-strip">
      <div><span>Status</span><strong>{{ formatStatus(detail.status) }}</strong></div>
      <div><span>Container</span><strong>{{ shortContainer(detail.container_id) }}</strong></div>
      <div><span>Provider</span><strong>{{ detail.provider_id }}</strong></div>
      <div><span>Tokens</span><strong>{{ totalTokens(detail) }}</strong></div>
      <div><span>Created</span><strong>{{ formatDate(detail.created_at) }}</strong></div>
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
                  <pre>{{ item.argumentsPreview }}</pre>
                </div>
                <div v-if="item.outputPreview" class="trace-preview">
                  <span>Output</span>
                  <pre>{{ item.outputPreview }}</pre>
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
                    <pre>{{ formatTraceValue(traceState(item).detail.arguments) }}</pre>
                  </div>
                  <div class="trace-block">
                    <span>Full output</span>
                    <pre>{{ formatTraceValue(traceState(item).detail.output) }}</pre>
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
import { computed, nextTick, reactive, watch } from 'vue'
import {
  formatStatus, formatDate,
  totalTokens, shortContainer, initial, roleInitial, roleLabel,
  statusTone, messageClass
} from '../utils/format'
import { renderMarkdown } from '../utils/markdown'
import { useApi } from '../composables/useApi'
import {
  buildAgentTimeline,
  formatDuration,
  formatTraceValue,
  toolStatusLabel
} from '../utils/timeline'

const props = defineProps({
  detail: { type: Object, default: null },
  events: { type: Array, required: true },
  draft: { type: String, default: '' },
  loading: { type: Boolean, default: false },
  sending: { type: Boolean, default: false }
})

const conversationRef = defineModel('conversationRef', { default: null })
const emit = defineEmits(['cancel', 'delete', 'send', 'update:draft'])
const { api, showToast } = useApi()
const expandedTools = reactive({})
const traces = reactive({})
const emptyTrace = { loading: false, error: '', detail: null }

const timelineItems = computed(() => buildAgentTimeline(props.detail, props.events))

watch(
  () => props.detail?.id,
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
    state.detail = await api(`/agents/${props.detail.id}/tool-calls/${encodeURIComponent(item.callId)}`)
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
