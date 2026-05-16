<template>
  <div class="timeline-row-card timeline-message" :class="messageClass(item.role)">
    <span class="message-marker" aria-hidden="true">{{ marker }}</span>
    <div class="message-content">
      <div class="message-header">
        <strong>{{ roleLabel(item.role) }}</strong>
        <span v-if="item.streaming">Streaming</span>
      </div>
      <div class="markdown-body timeline-message-body" v-html="renderMarkdown(item.content)"></div>
    </div>
  </div>
</template>

<script setup>
import { computed } from 'vue'
import { messageClass, roleLabel } from '../utils/format'
import { renderMarkdown } from '../utils/markdown'

const props = defineProps({
  item: { type: Object, required: true }
})

const marker = computed(() => {
  const role = String(props.item.role || '').toLowerCase()
  if (role === 'user') return '›'
  if (role === 'system') return '!'
  if (role === 'tool') return '$'
  return '•'
})
</script>
