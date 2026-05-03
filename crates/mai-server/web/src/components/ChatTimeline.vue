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
        <div v-else-if="item.type === 'error'" class="timeline-error-card">
          <strong>{{ item.title || 'Error' }}</strong>
          <p>{{ item.message }}</p>
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

defineProps({
  timelineItems: { type: Array, default: () => [] },
  loading: { type: Boolean, default: false },
  isToolExpanded: { type: Function, required: true },
  traceState: { type: Function, required: true }
})

defineEmits(['toggle-tool'])

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
