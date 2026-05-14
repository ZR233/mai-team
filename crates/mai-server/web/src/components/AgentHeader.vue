<template>
  <header class="agent-header">
    <div class="agent-identity">
      <div class="large-avatar">{{ initial(detail.name) }}</div>
      <div class="agent-title">
        <div class="title-row">
          <h2>{{ detail.name }}</h2>
          <span class="status-pill" :class="statusTone(detail.status)">
            {{ formatStatus(detail.status) }}
          </span>
        </div>
        <p v-if="detail.docker_image">
          <span>Docker</span>
          <span>/</span>
          <span class="mono">{{ detail.docker_image }}</span>
        </p>
        <p v-if="detail.last_error" class="error-text">{{ detail.last_error }}</p>
      </div>
    </div>

    <div class="agent-controls">
      <slot name="model-picker"></slot>
      <div class="agent-actions">
        <button v-if="showActions" class="ghost-button" type="button" @click="$emit('cancel', detail.id)">Cancel</button>
        <button v-if="showActions" class="danger-button" type="button" @click="$emit('delete', detail.id, detail.name)">Delete</button>
      </div>
    </div>
  </header>
</template>

<script setup>
import { formatStatus, initial, statusTone } from '../utils/format'

defineProps({
  detail: { type: Object, required: true },
  showActions: { type: Boolean, default: true }
})

defineEmits([
  'cancel',
  'delete'
])
</script>
