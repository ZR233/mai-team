<template>
  <div class="timeline-row-card todo-list-row">
    <div class="todo-list-header">
      <span class="process-dot todo-header-dot"></span>
      <span class="todo-list-title">Todo List</span>
      <span v-if="item.items.length" class="todo-list-meta">{{ completedCount }} / {{ item.items.length }}</span>
    </div>
    <div v-if="item.items.length" class="todo-list-items">
      <div
        v-for="(todo, index) in item.items"
        :key="index"
        class="todo-item"
        :class="`todo-${todo.status}`"
      >
        <span class="todo-status-dot"></span>
        <span class="todo-step">{{ todo.step }}</span>
      </div>
    </div>
  </div>
</template>

<script setup>
import { computed } from 'vue'

const props = defineProps({
  item: { type: Object, required: true }
})

const completedCount = computed(() =>
  props.item.items.filter((todo) => todo.status === 'completed').length
)
</script>
