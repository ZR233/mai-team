<template>
  <div class="context-status-line">
    <div class="status-line-left">
      <span class="status-line-state">
        <span class="status-dot" :class="statusTone(detail.status)"></span>
        <strong>{{ formatStatus(detail.status) }}</strong>
      </span>
      <span v-if="activity" class="status-line-activity">{{ activity }}</span>
    </div>

    <div class="status-line-items">
      <span class="status-line-item">
        <span>Model</span>
        <strong>{{ detail.provider_id === 'unconfigured' ? 'Not configured' : detail.model }}</strong>
      </span>
      <span v-if="detail.reasoning_effort" class="status-line-item">
        <span>Reasoning</span>
        <strong>{{ reasoningLabel(detail.reasoning_effort) }}</strong>
      </span>
      <span class="status-line-item token-status-chip" tabindex="0" :aria-label="tokenCacheTooltipLabel(detail)">
        <span>Tokens</span>
        <strong>{{ totalTokens(detail) }}</strong>
        <span class="token-cache-tooltip" role="tooltip" aria-hidden="true">
          <span
            v-for="row in tokenCacheTooltipRows(detail)"
            :key="row.label"
            class="token-cache-tooltip-row"
          >
            <span>{{ row.label }}</span>
            <strong>{{ row.value }}</strong>
          </span>
        </span>
      </span>
      <span
        v-if="contextCapacity.visible"
        class="status-line-item context-status-chip"
        :class="contextCapacity.tone"
      >
        <span>Context</span>
        <strong>{{ contextCapacity.used }} / {{ contextCapacity.total }}</strong>
        <span class="inline-context-meter" :style="{ '--compact-threshold': `${contextCapacity.threshold}%` }">
          <span :style="{ width: `${contextCapacity.percent}%` }"></span>
        </span>
      </span>
    </div>
  </div>
</template>

<script setup>
import { formatStatus, statusTone, tokenCacheTooltipLabel, tokenCacheTooltipRows, totalTokens } from '../utils/format'
import { reasoningLabel } from '../utils/reasoning'

defineProps({
  detail: { type: Object, required: true },
  contextCapacity: { type: Object, required: true },
  activity: { type: String, default: '' }
})
</script>
