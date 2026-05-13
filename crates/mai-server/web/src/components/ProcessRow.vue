<template>
  <div class="timeline-row-card process-row" :class="`process-${item.tone}`">
    <span class="process-dot"></span>
    <span class="process-label">{{ item.label || item.title }}</span>
    <span v-if="item.detail || item.summary" class="process-detail">{{ item.detail || item.summary }}</span>
  </div>
</template>

<script setup>
defineProps({
  item: { type: Object, required: true }
})
</script>
