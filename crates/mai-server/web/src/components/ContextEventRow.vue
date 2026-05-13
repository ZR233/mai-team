<template>
  <div class="timeline-row-card context-event-row">
    <div class="timeline-row-content">
      <div class="timeline-row-header">
        <div class="timeline-row-title">
          <strong>{{ item.title || 'Context updated' }}</strong>
          <span v-if="item.summary">{{ item.summary }}</span>
        </div>
      </div>
      <p v-if="item.detail" class="timeline-row-detail">{{ item.detail }}</p>
    </div>
  </div>
</template>

<script setup>
defineProps({
  item: { type: Object, required: true }
})
</script>
