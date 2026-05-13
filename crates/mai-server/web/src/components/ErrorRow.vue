<template>
  <div class="timeline-row-card error-row">
    <div class="timeline-row-content">
      <div class="timeline-row-header">
        <div class="timeline-row-title">
          <strong>{{ item.title || 'Error' }}</strong>
          <span v-if="item.message">{{ item.message }}</span>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup>
defineProps({
  item: { type: Object, required: true }
})
</script>
