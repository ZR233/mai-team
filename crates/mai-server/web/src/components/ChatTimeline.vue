<template>
  <section class="conversation chat-timeline" ref="localConversationRef">
    <div v-if="loading" class="loading-center">
      <div class="spinner"></div>
      <span>Loading messages...</span>
    </div>
    <div v-else-if="!timelineItems.length" class="quiet-empty">
      <template v-if="statusItem">
        <ProcessRow :item="statusItem" />
      </template>
      <template v-else>
        <strong>No messages yet</strong>
        <span>Send the first instruction to start a turn.</span>
      </template>
    </div>
    <template v-else>
      <article
        v-for="item in timelineItems"
        :key="item.key"
        class="timeline-item"
        :class="timelineItemClasses(item)"
      >
        <ChatMessage v-if="item.type === 'message'" :item="item" />
        <SkillCallRow v-else-if="item.type === 'skill_call'" :item="item" />
        <ToolCallRow
          v-else-if="isToolItem(item)"
          :item="item"
          :expanded="isToolExpanded(item)"
          :trace-state="traceState(item)"
          @toggle="$emit('toggle-tool', item)"
        />
        <ContextEventRow v-else-if="item.type === 'context_event'" :item="item" />
        <TodoListRow v-else-if="item.type === 'todo_list'" :item="item" />
        <UserInputRow v-else-if="item.type === 'user_input'" :item="item" />
        <ErrorRow v-else-if="item.type === 'error'" :item="item" />
        <ArtifactRow v-else-if="item.type === 'artifact'" :item="item" />
        <ProcessRow v-else :item="item" />
      </article>
    </template>
  </section>
</template>

<script setup>
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import ArtifactRow from './ArtifactRow.vue'
import ChatMessage from './ChatMessage.vue'
import ContextEventRow from './ContextEventRow.vue'
import ErrorRow from './ErrorRow.vue'
import ProcessRow from './ProcessRow.vue'
import SkillCallRow from './SkillCallRow.vue'
import ToolCallRow from './ToolCallRow.vue'
import TodoListRow from './TodoListRow.vue'
import UserInputRow from './UserInputRow.vue'
import { timelineItemClasses } from '../utils/timeline'

const props = defineProps({
  timelineItems: { type: Array, default: () => [] },
  loading: { type: Boolean, default: false },
  statusItem: { type: Object, default: null },
  isToolExpanded: { type: Function, required: true },
  traceState: { type: Function, required: true }
})

const statusItem = computed(() => props.statusItem)

defineEmits(['toggle-tool'])

function isToolItem(item) {
  return item.type === 'tool_call' || item.type === 'tool'
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
