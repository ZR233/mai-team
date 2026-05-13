<template>
  <div class="timeline-row-card artifact-row">
    <a
      :href="`/artifacts/${artifact.id}/download`"
      download
      class="artifact-download"
      :title="`Download ${artifact.name}`"
    >
      <span class="artifact-name">{{ artifact.name }}</span>
      <span v-if="formatBytes(artifact.size_bytes)" class="artifact-size">
        {{ formatBytes(artifact.size_bytes) }}
      </span>
      <svg class="artifact-icon" viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
        <path d="M8 1.5v8m0 0 3-3m-3 3-3-3" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" />
        <path d="M3 11.5v1.25c0 .41.34.75.75.75h8.5c.41 0 .75-.34.75-.75V11.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" />
      </svg>
    </a>
  </div>
</template>

<script setup>
import { computed } from 'vue'

const props = defineProps({
  item: { type: Object, required: true }
})

const artifact = computed(() => props.item.artifact || {})

function formatBytes(value) {
  if (!value || !Number.isFinite(value)) return ''
  if (value < 1024) return `${value} B`
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`
  return `${(value / 1024 / 1024).toFixed(1)} MB`
}
</script>
