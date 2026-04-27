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
      <section class="conversation" ref="conversationRef">
        <div v-if="loading" class="loading-center">
          <div class="spinner"></div>
          <span>Loading messages...</span>
        </div>
        <div v-else-if="!detail.messages?.length" class="quiet-empty">
          <strong>No messages yet</strong>
          <span>Send the first instruction to start a turn.</span>
        </div>
        <template v-else>
          <article
            v-for="(message, index) in detail.messages"
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
          <span class="event-count">{{ events.length }}</span>
        </div>
        <div class="event-list">
          <div
            v-for="event in events"
            :key="event.sequence"
            class="event-row"
            :class="`event-${event.type || 'unknown'}`"
          >
            <div class="event-row-head">
              <span class="event-type">{{ formatEventType(event.type) }}</span>
              <time>{{ formatTime(event.timestamp) }}</time>
            </div>
            <p>{{ eventSummary(event, formatStatus, roleLabel) }}</p>
          </div>
          <div v-if="!events.length" class="quiet-empty compact">
            <strong>No events yet</strong>
            <span>Lifecycle and tool events appear here.</span>
          </div>
        </div>
      </aside>
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
import {
  formatStatus, formatDate, formatTime, formatEventType,
  totalTokens, shortContainer, initial, roleInitial, roleLabel,
  statusTone, messageClass, eventSummary
} from '../utils/format'
import { renderMarkdown } from '../utils/markdown'

const props = defineProps({
  detail: { type: Object, default: null },
  events: { type: Array, required: true },
  draft: { type: String, default: '' },
  loading: { type: Boolean, default: false },
  sending: { type: Boolean, default: false }
})

const conversationRef = defineModel('conversationRef', { default: null })

const emit = defineEmits(['cancel', 'delete', 'send', 'update:draft'])

function handleEnter(event) {
  if (!event.shiftKey) {
    event.preventDefault()
    send()
  }
}

function send() {
  if (props.draft.trim()) emit('send', props.draft.trim())
}
</script>
