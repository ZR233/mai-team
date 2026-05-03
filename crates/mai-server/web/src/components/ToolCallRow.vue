<template>
  <div class="tool-row" :class="[`tool-${item.status}`, { open: expanded }]">
    <button class="tool-row-toggle" type="button" @click="$emit('toggle')">
      <svg class="tool-chevron" :class="{ open: expanded }" viewBox="0 0 16 16" aria-hidden="true">
        <path d="M6 4l4 4-4 4" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" />
      </svg>
      <span class="tool-state-dot"></span>
      <span class="tool-row-main">
        <span>{{ item.toolActionLabel || toolStatusLabel(item.status) }}</span>
        <strong :title="item.toolPrimary || item.toolName">{{ item.toolPrimary || item.toolName }}</strong>
        <small v-if="item.toolSecondary">{{ item.toolSecondary }}</small>
      </span>
      <span class="tool-meta">
        {{ toolStatusLabel(item.status) }}
        <template v-if="formatDuration(item.durationMs)"> · {{ formatDuration(item.durationMs) }}</template>
      </span>
    </button>

    <div v-if="item.toolPreviewLines?.length" class="tool-output-tail">
      <pre>{{ item.toolPreviewLines.join('\n') }}</pre>
    </div>

    <div v-if="expanded" class="tool-trace">
      <div v-if="traceState.loading" class="trace-loading">
        <span class="spinner-sm"></span>
        Loading full trace...
      </div>
      <p v-else-if="traceState.error" class="trace-error">
        {{ traceState.error }}
      </p>
      <template v-else-if="traceState.detail">
        <div class="trace-block">
          <span>Full arguments</span>
          <div
            class="trace-surface"
            v-html="renderToolTrace({ toolName: item.toolName, kind: 'arguments', value: traceState.detail.arguments })"
          ></div>
        </div>
        <div class="trace-block">
          <span>Full output</span>
          <div
            class="trace-surface"
            v-html="renderToolTrace({ toolName: item.toolName, kind: 'output', value: traceState.detail.output })"
          ></div>
        </div>
      </template>
      <template v-else>
        <div v-if="item.argumentsPreview" class="trace-block">
          <span>Arguments</span>
          <div
            class="trace-surface"
            v-html="renderToolTrace({ toolName: item.toolName, kind: 'arguments', value: item.argumentsPreview })"
          ></div>
        </div>
        <div v-if="item.outputPreview" class="trace-block">
          <span>Output</span>
          <div
            class="trace-surface"
            v-html="renderToolTrace({ toolName: item.toolName, kind: 'output', value: item.outputPreview })"
          ></div>
        </div>
      </template>
    </div>
  </div>
</template>

<script setup>
import {
  formatDuration,
  renderToolTrace,
  toolStatusLabel
} from '../utils/timeline'

defineProps({
  item: { type: Object, required: true },
  expanded: { type: Boolean, default: false },
  traceState: { type: Object, required: true }
})

defineEmits(['toggle'])
</script>
