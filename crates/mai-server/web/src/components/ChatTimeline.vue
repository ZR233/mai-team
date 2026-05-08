<template>
  <section class="conversation chat-timeline" ref="localConversationRef">
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
        :class="`timeline-${item.type}-item`"
      >
        <ChatMessage v-if="item.type === 'message'" :item="item" />
        <ToolCallRow
          v-else-if="item.type === 'tool'"
          :item="item"
          :expanded="isToolExpanded(item)"
          :trace-state="traceState(item)"
          @toggle="$emit('toggle-tool', item)"
        />
        <TodoListRow v-else-if="item.type === 'todo_list'" :item="item" />
        <div v-else-if="item.type === 'user_input'" class="user-input-card">
          <strong v-if="item.header">{{ item.header }}</strong>
          <div v-for="q in item.questions" :key="q.id" class="user-input-question">
            <p>{{ q.question }}</p>
            <div class="user-input-options">
              <div v-for="opt in q.options" :key="opt.label" class="user-input-option">
                <span class="option-label">{{ opt.label }}</span>
                <span v-if="opt.description" class="option-desc">{{ opt.description }}</span>
              </div>
            </div>
          </div>
        </div>
        <div v-else-if="item.type === 'error'" class="timeline-error-card">
          <strong>{{ item.title || 'Error' }}</strong>
          <p>{{ item.message }}</p>
        </div>
        <div v-else-if="item.type === 'artifact'" class="artifact-card">
          <a :href="`/artifacts/${item.artifact.id}/download`" download class="artifact-download" :title="`Download ${item.artifact.name}`">
            <svg class="artifact-icon" viewBox="0 0 16 16" width="18" height="18" fill="currentColor"><path d="M8 1a.75.75 0 0 1 .75.75v6.69l2.22-2.22a.75.75 0 1 1 1.06 1.06l-3.5 3.5a.75.75 0 0 1-1.06 0l-3.5-3.5a.75.75 0 0 1 1.06-1.06l2.22 2.22V1.75A.75.75 0 0 1 8 1Z"/><path d="M2.5 10.75a.75.75 0 0 1 1.5 0v1.75c0 .138.112.25.25.25h7.5a.25.25 0 0 0 .25-.25v-1.75a.75.75 0 0 1 1.5 0v1.75A1.75 1.75 0 0 1 11.75 14.5h-7.5A1.75 1.75 0 0 1 2.5 12.75v-1.75Z"/></svg>
            <span class="artifact-name">{{ item.artifact.name }}</span>
            <span class="artifact-size">{{ formatBytes(item.artifact.size_bytes) }}</span>
          </a>
        </div>
        <ProcessRow v-else :item="item" />
      </article>
    </template>
  </section>
</template>

<script setup>
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'
import ChatMessage from './ChatMessage.vue'
import ProcessRow from './ProcessRow.vue'
import ToolCallRow from './ToolCallRow.vue'
import TodoListRow from './TodoListRow.vue'

defineProps({
  timelineItems: { type: Array, default: () => [] },
  loading: { type: Boolean, default: false },
  isToolExpanded: { type: Function, required: true },
  traceState: { type: Function, required: true }
})

defineEmits(['toggle-tool'])

function formatBytes(value) {
  if (!value || !Number.isFinite(value)) return ''
  if (value < 1024) return `${value} B`
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`
  return `${(value / 1024 / 1024).toFixed(1)} MB`
}

const conversationRef = defineModel('conversationRef', { default: null })
const localConversationRef = ref(null)

onMounted(() => {
  conversationRef.value = localConversationRef.value
})

watch(localConversationRef, (value) => {
  conversationRef.value = value
})

onBeforeUnmount(() => {
  if (conversationRef.value === localConversationRef.value) conversationRef.value = null
})
</script>
