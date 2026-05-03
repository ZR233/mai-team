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
        <p>
          <span>{{ detail.provider_name }}</span>
          <span>/</span>
          <span class="mono">{{ detail.model }}</span>
        </p>
        <p v-if="detail.docker_image">
          <span>Docker</span>
          <span>/</span>
          <span class="mono">{{ detail.docker_image }}</span>
        </p>
        <p v-if="detail.last_error" class="error-text">{{ detail.last_error }}</p>
      </div>
    </div>

    <div class="agent-controls">
      <div class="thinking-depth-control" :class="{ disabled: !reasoningOptions.length }">
        <label for="thinking-depth-select">
          <span>思考深度</span>
          <select
            id="thinking-depth-select"
            :value="reasoningEffort"
            :disabled="!reasoningOptions.length || modelChangeBusy || updatingModel"
            @change="$emit('change-reasoning-effort', $event.target.value)"
          >
            <option v-if="!reasoningOptions.length" value="">
              当前模型不支持
            </option>
            <option v-for="option in reasoningOptions" :key="option.value" :value="option.value">
              {{ option.label }}
            </option>
          </select>
        </label>
        <small v-if="reasoningOptions.length">
          {{ updatingModel ? '保存中...' : '下一轮生效' }}
        </small>
        <small v-else>模型不支持</small>
      </div>

      <div class="agent-actions">
        <button
          class="ghost-button"
          type="button"
          :disabled="!providersCount || modelChangeBusy || updatingModel"
          @click="$emit('open-model')"
        >
          Change Model
        </button>
        <button class="ghost-button" type="button" @click="$emit('cancel', detail.id)">Cancel</button>
        <button class="danger-button" type="button" @click="$emit('delete', detail.id, detail.name)">Delete</button>
      </div>
    </div>
  </header>
</template>

<script setup>
import { formatStatus, initial, statusTone } from '../utils/format'

defineProps({
  detail: { type: Object, required: true },
  reasoningEffort: { type: String, default: '' },
  reasoningOptions: { type: Array, default: () => [] },
  providersCount: { type: Number, default: 0 },
  modelChangeBusy: { type: Boolean, default: false },
  updatingModel: { type: Boolean, default: false }
})

defineEmits([
  'change-reasoning-effort',
  'open-model',
  'cancel',
  'delete'
])
</script>
