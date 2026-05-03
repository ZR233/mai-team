<template>
  <div class="timeline-message" :class="messageClass(item.role)">
    <div class="message-marker" aria-hidden="true">{{ marker }}</div>
    <div class="message-content">
      <span>{{ roleLabel(item.role) }}</span>
      <div class="markdown-body" v-html="renderMarkdown(item.content)"></div>
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
