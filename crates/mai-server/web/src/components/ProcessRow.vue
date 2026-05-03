<template>
  <div class="process-row" :class="`process-${item.tone}`">
    <span class="process-dot"></span>
    <span class="process-label">{{ item.label }}</span>
    <span v-if="item.detail" class="process-detail">{{ item.detail }}</span>
  </div>
</template>

<script setup>
defineProps({
  item: { type: Object, required: true }
})
</script>
